# Roadmap

**In one line:** what is being built next, and why. Nothing here is required to
run Storycast; if you came here to make audio, the [README](../README.md) is
where you want to be.

Plans in their author's words, restated in mine. If I misread anything, correct
the wording here. This file is the contract the work gets built against.

Status legend: `planned` (written down, not started), `in progress`, `done`.

## 1. Any key, any endpoint, any models: de-hardcode the analyzer (`done`)

**Your words:** "change hardcoded gemini key to any key with api link + model
names."

**What shipped.** Providers are data, not code. `.bm/llm.json` (machine-global,
seeded once from the tracked `llm.default.json`) holds an entry per provider —
`kind`, `base_url`, `api_key`, `model` — and `active` names the one in use. The
`L` (`:llm`) screen edits it, `f` asks the provider what models it serves, and
adding a gateway by hand means writing one entry and nothing else: an unknown
`kind` rides the OpenAI-compatible path.

Three `kind`s exist, and they are protocols rather than vendors:

| `kind` | Wire | Endpoint field |
| --- | --- | --- |
| `gemini` | native `:generateContent` REST | `gemini_url` |
| `openai` | `POST {base}/chat/completions` | `openrouter_url` |
| `ollama` | `POST {base}/api/chat` | `ollama_url` |

Only the protocol decides routing (`LlmKind::parse`, keyed on `kind`, never on
the id), so `tokenharbor`, `my-gateway` and `openrouter` are one code path. The
model, the endpoint and the slot travel to whichever box digests
(`AnalyzerSettings`), and the key rides `Credentials` narrowed to that one slot
by `for_stage` — a crawl offer carries no secret at all.

**Four places the written plan was wrong and the code went another way.**

- **`.env` was retired rather than extended.** The plan put
  `LLM_BASE_URL`/`LLM_API_KEY`/`LLM_MODELS` in it. The file was already the
  wrong home: a key is this machine's access, not a book's, and the workspace's
  `settings.json` is per book. `.bm/llm.json` is the one place, and
  `load_or_seed` reads the old `.env` **once** to carry its keys over so nobody
  re-types them. Nothing loads it at startup any more.
- **The credential keeps its two names, and that is the design.**
  `GEMINI_API_KEY`/`OPENROUTER_API_KEY` are the *wire's* per-slot vocabulary —
  the names a worker installs from the offer, one key per stage — not names the
  operator types. A `LLM_API_KEY` would have to be threaded through
  `Credentials`, the agent's installer and the sidecar to say one fewer word,
  and the render lane's Gemini key is a different credential for a different
  purpose. The operator never meets a vendor name unless they go looking.
- **The endpoint that was actually hardcoded was the Gemini one.** This is the
  part that was still broken when the rest of the plan was already true:
  `try_gemini_model` built Google's URL itself and ignored its entry's
  `base_url`, so the `L` screen listed models off the operator's gateway and
  every generation then went to a host that had never seen the key. The base is
  the entry's now, on the wire and in the request.
- **The key moved out of the URL.** Google's own form is the `x-goog-api-key`
  header; `?key=` was putting a credential into something that gets logged,
  echoed in errors and read by any proxy in between. `fetch_models` uses the
  header too, so a listing and the generation that follows it are the same
  request against the same host.
- **`--api` became optional, and that was not cosmetic.** `bm-inductor backup`
  defaulted it to `https://openrouter.ai/api/v1` and then wrote it over the
  chosen slot's endpoint, so `--analyzer tokenharbor` (or a gemini gateway)
  without `--api` called OpenRouter's address with someone else's key. An
  omitted flag now means the provider's own entry, and `--api` lands on the
  Gemini slot too — it used to be dropped for anything but `openai`, on the
  request *and* in the backup's opening line, which printed Google's host while
  the requests went elsewhere.

The fallback semantics are unchanged: per-model attempts, retry after the
provider's own delay on 429, skip on 404/day-quota, abort on 401/403/400.

The OpenRouter `Referer`/`X-Title` pair that named this repo is gone — the slot
serves every gateway, so nothing branded rides a request. Pinned, with the
endpoint and the key's placement, by three fixture-server tests in
`digest/llm.rs` plus two wiring tests in `config.rs` (no API keys, no network:
loopback only).

Files: `rust/crates/bm-core/src/config.rs`,
`rust/crates/bm-core/src/digest/llm.rs`, `rust/crates/bm-proto/src/lib.rs`,
`rust/crates/bm-inductor/src/main.rs`, `llm.default.json`.

## 2. AWS: workers on EC2 (`done`)

**Your words:** "connect to AWS". Confirmed and shipped as: **EC2 for compute,
driven entirely from the TUI**. Artifacts and output stay where they are today
(the inductor's disk and each box's segment store); no S3, and none is planned.
If a cloud object store ever becomes necessary it would be a new item here,
not a revival of this one.

The shape is close to what was first written down, with three places where the
plan was wrong and the code went another way. Those are worth keeping, because
each one was a real correction:

- **A provisioned instance is not a special kind of box.** The plan said "the
  provisioner learns an AWS target alongside the `ssh` target". It did not
  need to: an instance is linked with the pool's `.pem` and the `ubuntu` login
  and then provisioned by exactly the same `ssh`/`rsync` path as a LAN box,
  stamp logic included. `:up` is the only AWS-specific step, and all it does is
  launch and link. Guides: [AWS-WORKERS.md](AWS-WORKERS.md),
  [AWS-IAM-USER.md](AWS-IAM-USER.md).
- **A GPU instance for the render lane was the wrong idea, and it is not
  planned.** The plan assumed render was the expensive stage in a
  GPU-accelerable sense. It is not: the TTS path is a hand-written SIMD matvec
  with no GPU code, so the instance choice is decided by **RAM, not CPU**. The
  sidecar is about 2.85 GB resident the moment the weights load, so 8 GiB is
  the size and 4 GiB does not fit. See [AWS-WORKERS.md](AWS-WORKERS.md) §4
  for the measurement.
- **The instance id lives in the machine's note, not in a new field.** The
  plan said `.bm/machines.json` "needs an `instance-id` next to addr/user/key".
  The registry still keys by **address**, and the id is stamped in the note
  instead. That is what lets `state/relink.rs` repair an address that rotated
  (every stop/start, every spot relaunch) by matching the stable id against
  one account listing. A field would have been the second source of truth this
  repo keeps avoiding.

The cost guardrail is not one mechanism but three, because they fail
differently: `Settings.idle_mins` (default 5) shuts the cluster down when there
is genuinely nothing to do, `X` stops everything on command, and `:down`
terminates the boxes, asking first, refusing while a render is in flight, and
scoped to the `storycast-worker` tag **and** explicit instance ids.
`ttl_hours` (6) is the backstop for a box that outlives its work.

Credentials are exactly where the plan said they must be: the IAM user's key in
the ignored `.bm/aws/credentials` (0600), never in `machines.json`, and with
**no fallback to this machine's own AWS identity**.

Where the cloud plane lives, kept as a map now that the work is done:

| | |
|---|---|
| `bm-core/src/provision/aws.rs` | the EC2 calls, the AMI lookup through SSM, the firewall check |
| `bm-core/src/provision/aws_credentials.rs` | the credential store, and the verify-then-write order |
| `bm-core/src/provision/ssh.rs` | `HOST_KEY_OPTS`: one constant, both transports |
| `bm-core/src/provision/steps.rs` | `may_install`, and the stamp the second run skips on |
| `bm-inductor/src/aws_ops.rs` | one implementation per verb, shared by CLI and TUI |
| `bm-inductor/src/dispatch.rs` | the inductor-drives loop |
| `bm-inductor/src/state/{observe,relink}.rs` | one entry point for liveness; address drift repair |
| `bm-inductor/src/tui/{draw,input}/{cloud,policy}.rs` | the Cloud view and the policy view |
| `aws.default.json`, `aws-policy.json` | the tracked pool shape, and the policy to paste |

## 3. A workspace pack: one live pack per book (`in progress`)

**Your words:** "no need to do a workspace until the settings are completed."

**What it is.** There is exactly **one live pack** and every book shares it.
`Layout::assets()` is the *checkout's*, unlike `prompts/` — which is
work-scoped, so each book can already have its own. [ARCHITECTURE.md §The
map](ARCHITECTURE.md#the-map-the-book-the-machine-and-what-each-one-borrows)
already names this as the box that changes what a book sounds. The target: a
workspace's pack **extends its profile's pack one-to-one** and adds the book's
own taste and presets on top, so two books do not share a score.

**What shipped, 2026-10-01.** The first step, and most of what this item
asked for. `Layout::assets()` is **work-scoped** — the active workspace's own
`assets/` when it has one, the checkout's when it does not — and
`workspace new --profile <preset>` composes that tree at creation from the
preset's `pack_deps` (`profiles/presets.json`; see
[PROFILES.md §Choosing a profile](PROFILES.md#choosing-a-profile-presets-and-the-workspaces-own-pack)).
`the-apothecary-diaries` runs its own `common + craft + court-mystery`
composition beside `beyond-myriads`' checkout-level `xianxia` tree, and the
composition resolved on a real book end to end — the crawl through the digest —
with no change to the checkout's files. Not done yet: a workspace releasing
its own composition as a bundle (`profile manifest <name> --piece pack`
currently hashes whichever tree is in force, so the verb already works for the
active workspace; the tag/release flow around it is untested), and a binding
stamp beside the workspace cache so a hand-edited `settings.json` cannot point
a book at another book's score silently.

**What I did first, and why it was the right order.** The two packs
(`craft`, `court-mystery`) were built and gated *before* this, so the shape a
layout change has to carry is a known one rather than a sketch. That turned out
to matter more than expected: the gate found on its first run that **a preset
must answer the entire palette it inherits**, because the mood vocabulary lives
in a root and no root holds a score. That is a property of the data model, not
of the workspace question, and it is now written down in
[SOUND.md §8](SOUND.md#8-the-mood-vocabulary-and-how-it-grows). It mattered
again: composing the two packs for a real book was what surfaced the alias
hazards (an alias a root ships that a stronger pack's own vocabulary makes
canonical) that no per-pack gate can see, because only a composition folds
both.

**What I'd do first.** ~~Make `Layout::assets()` work-scoped the way
`prompts/` already is, reading the workspace's own `pack.json` and falling back
to the profile's.~~ Done — everything else followed from that one line.

## 4. The music loop seam becomes a property, not a fix (`planned`)

**Your words:** part of the Apothecary sound-design plan, §8.1.

**What shipped.** A music run longer than its track was rendered with
`-stream_loop -1` — a hard butt-join — while the inject layer already had
`loop_copies` / `loop_filter` for exactly this. The seam is not theory: a test
clip with a 50 ms end fade measured **−44.0 dB at the seam against −43.5 dB
either side of it**, once every couple of minutes, for the length of a chapter.
The music path now uses the same crossfade, with the pause-lift gain expression
on the loop's tail so a looping track still lifts inside a beat.

**What is left.** The *fix* is in; the **property** is not. State it — "a music
track is always rendered as a crossfaded loop, never butt-joined" — in
[SOUND.md](SOUND.md), and assert it with the test that already exists, so a
future edit to the music path cannot quietly reintroduce `-stream_loop`. A
`grep` in CI would be cruder and would rot; the test is the honest version.

## 5. CI that can see a real pack (`planned`)

**Your words:** part of the Apothecary sound-design plan, §8.2.

**What shipped.** `cargo test -p bm-core --test pack_gates` walks every pack
under `assets/_extends/`, resolves each in a scratch root, and checks that every
palette value but `none` is answered by a track, every rule's effect tags are
answered by a bed, and every named file is present or not-yet-recorded. It found
two real defects in a real pack on its first run — a palette value inherited
without an answer, and two effect tags with no bed, both of which are silent
failures with no error anywhere.

**What is left.** That gate only runs on a machine that *has* the tree, and
`/assets/*` is git-ignored — so a fresh clone gets the fixture and nothing else.
The durable version is a **fixture profile that mirrors the real palette
vocabulary**, so `cargo test` on a clean checkout catches a mood with no track
with no `assets/` present at all. That is the difference between a convention and
a gate, and it is the same class of change as item 3: worth doing, not worth
rushing.

## 6. Carried forward from ASSETS.md's own list (`planned`)

Four things that file already names as undecided and that **sound design hits
first**, because a pack is mostly sound design. All in
[ASSETS.md §What is not decided here](ASSETS.md#what-is-not-decided-here), restated
here so they are not lost:

* **`asset adopt <rel>`** — what marks an inherited file as the dependency's is
  the composition record, so a tree that decides to *own* a file it inherited
  without editing it has to prune the marker by hand. This is what happens the
  day a preset wants a track `common` shipped.
* **`asset import <file>`** — what would make the External Library box real.
* **An explicit `shadowed` list** — a genre can shadow an inherited rule by
  restating its match set, but the world's entry stays in the tree. A way to say
  "not here" rather than restating it forever.
* **A screen showing "inherited from `common`" beside "yours"** — so editing the
  resolved tree is visible before it is overwritten. The authoring answer is
  already decided ([ASSET-PACKS.md](ASSET-PACKS.md#where-the-art-is-edited): never
  edit the resolved tree), and this is the UI that would make the mistake
  impossible rather than merely discouraged.

## 7. A chapter's timing becomes a sidecar, and an act renders to video (`in progress`)

**Your words:** "could you do the actual video render now? well after editing the
merge I guess."

**My reading.** The pipeline's job is audio and it stays that way: the one
change it makes is to *record* what it already knows. A standalone tool turns
that record plus the mp3 into a video, and DaVinci Resolve does the finishing by
hand. See the approved surface in `video-layout-mock.html` and the layout in
`tools/video-template.json`.

**What shipped.**

- **The cue sidecar.** Merge now writes `output/Ch.N - Title.cues.json` beside
the published mp3 — every script segment's `speaker`, `start`, `end` and `text`
on the delivered clock (`assemble/cues.rs`, built after `retime`, moved into
`output/` by `publish`). One cue per segment, never per TTS call, so captions do
not merge two sentences that were spoken as one. The merge replays nothing and no
other stage moves; the timing is captured where it is already computed.
- **`tools/video.py`.** Reads an acts manifest
  (`{acts: [{act, title, chapters}]}`), the chapters' sidecars and mp3s, and
  writes **one** video for the whole run — `acts-NN-NN.{mp4,srt,vtt}`. `--preview
  N`, `--dry-run`, `--chapter-gap`, `--no-subs` and `--template` are the knobs.
- **The template JSON drives the render.** `tools/video-template.json` is the
same object the mock was built from — palette, zones with the `align_top` /
`align_bottom` edge rules, the type unit, the act-label `Hồi #` pattern, the
timeline geometry. Colour strings stay CSS (`#hex`,
`radial-gradient(...)`, `rgba(...)`) and the tool parses them.
- **Captions are cut *in the tool*.** One per cue, broken on sentence ends and
  wrapped to the frame, 1–7 s two lines, and a two-line caption is justified —
  the rows come out flush rather than leaving one word stranded on the last.
  Anyone but the narrator is named: `Dịch Phong: "Ừm!"`. A dash in the line —
  `Đinh —— Quyền pháp` — is a pause the narrator takes, so it reads as
  `Đinh; Quyền pháp`. A caption fades up over its first frames and down over its
  last, over `subtitle.fade_s` (0.3 s, clamped to 0.1–0.5), and is on screen
  nowhere else — fading the change itself showed the next line before its own
  subtitle time, which read as a line appearing, vanishing in the gap between
  speakers, and reappearing.
- **A speaker portrait, not a speaker name.** `speaker_sticker` in the template
  maps a speaker to an image with a `fallback` for anyone unmapped, matched
  case-insensitively because the script and the filename disagree on `Hệ Thống`
  vs `Hệ thống`. The narrator gets nothing, and the caption carries no name — the
  portrait is the name. The portrait is scaled by the timeline thumb's own ratio
  — `radius * scale * 2` over the thumb image's own width — rather than fitted to
  a box, so it keeps its proportions. It sits **above the captions, centred on
  the frame** at `speaker_sticker.at`, not beside them: every caption is
  re-wrapped and re-centred, so a portrait in the gutter has to move with the
  text, and a badge sliding between two positions reads as leaving the line it
  belongs to. Fixed, it is simply there while its character talks.
- **The badge lives for a run, and reacts to its own speaker only.** Consecutive
  captions from one speaker are grouped, and the portrait fades in on the run's
  first frame and out on its last, solid throughout — fading per caption made a
  character who speaks three lines in a row blink twice per line. A run breaks
  when the speaker changes or a narrator caption intervenes. The squash comes
  from the speech envelope (`astats` RMS per video frame, instant attack and a
  0.14 s release, windowed on this chapter's own percentiles) **gated to that
  speaker's own cue frames**: the envelope is read off the mixed chapter, so
  ungated it made the portrait squashed to the narrator, to other characters and
  to the music bed. x scales by `-7%`, y by `+11%` at full loudness.
- **Caption tiles are transparent, and the strip is frame-exact.** A tile is
  glyphs on alpha, never a colour field, so the paper shows through; the strip is
  qtrle, the one encoder here that keeps the alpha channel. The fades are baked
  into blended tiles rather than left to a filter, so the composite stays one
  overlay however many captions there are, and they stop short of a fully
  transparent frame so no boundary flashes. Caption times are snapped to the
  frame grid and the sidecars carry those times, because the concat demuxer
  quantises a still's duration — a run of 0.033 s fade frames drifted the
  captions off their own timings — so the strip is piped as an explicit frame
  list instead. Sidecars are written beside the video as `.srt`/`.vtt`, so the
  burned copy is for watching and the sidecars are the retimable, translatable
  master.
- **One segment per act, one video.** The bar carries a segment per act, the
  current one sealed and raised; the act title and label swap at the act's
  boundary (each act is a timed tile ffmpeg overlays). A round-clipped thumb
  rides the whole bar, and when `timeline.thumb.single` names an image it spins
  at `spin_s` in the direction `direction` asks for (`ccw`, or the disc when no
  image is named). The illustration zone stays empty: it is where Resolve drops
  art, and a placeholder only invites tidying it away.

**Two constraints the plan did not anticipate.**

- **This box's ffmpeg has no `drawtext`/libass.** So text, the paper ground and
  the subtitle plates are rasterised with Pillow and composited; ffmpeg only
  draws the act bar and muxes the audio. That is the one new Python import the
  tool carries, and it is confined to `tools/`.
- **A `vieneu` run can be a whole paragraph, one wav.** The local engine folds a
  speaker's consecutive lines into one TTS call, so a turn holds several script
  segments; the cue sheet splits that turn's span between them by length. There
are no word timings anywhere in the pipeline, so a boundary inside a batched turn
can drift slightly from the speech — the escape hatch is tool-side forced
alignment, which does not change the pipeline contract.

**What is left.** The act manifests are one file each today; grouping a book
into acts from its ledger is not written. And the tool needs the three faces in
the git-ignored `tmp/`, so a fresh clone renders no text until they are pointed
elsewhere.
