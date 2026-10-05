# bm-video

Renders a Storycast book's acts into one video, from what the pipeline already
publishes: the merged `output/Ch.N - Title.mp3` files and their
`Ch.N - Title.cues.json` sidecars.

It is an **out-of-tree plugin**. It is not a member of the Rust workspace under
`rust/`, nothing in the pipeline depends on it, and it depends on nothing in the
pipeline — it reads files and writes a video. `cargo build --workspace` and
`make test` in `rust/` do not build, test or lint it. It has its own workspace
root here, its own `Cargo.lock` and its own `target/`.

It replaced `tools/video.py`, with the same template
(`tools/video-template.json`) and the same look.

## Build and run

```sh
cd plugins/bm-video
make release                     # optimized; a debug build renders ~10x slower
make test                        # cargo test + clippy -D warnings (this plugin only)

./target/release/bm-video \
  --acts ../../renders/acts.json \
  --outdir ../../renders
```

Run it from the repository root (`--root` defaults to the working directory),
because the template's `font_files`, `timeline.thumb` and `speaker_sticker`
paths resolve from there — the faces and portraits live in the git-ignored
`tmp/`. Needs `ffmpeg` and `ffprobe` on `PATH`.

Useful flags: `--preview SECS` (a timing check), `--chunk-secs`, `--jobs`,
`--rebuild` (ignore the cache), `--dry-run` (print the chunk plan), `--no-subs`.

## How a render works

The timeline is cut into fixed-length **chunks** (4 s by default). Each chunk is
composed in-process — paper ground, act plate, timeline bar, spinning thumb,
caption tiles, portrait — and piped as `rgb24` straight into one `ffmpeg` per
chunk. The finished chunks are concatenated with a stream copy, and the chapter
audio is muxed in. There is no intermediate video, no per-caption PNG on disk,
and one encode pass rather than two.

**Every chunk is content-addressed.** Its file name carries a hash of exactly
what decides its pixels: the template, the act windows, the captions and their
fade/portrait state in that frame range, the quantised speech envelope, and —
only for the speakers who appear in that chunk — the bytes of their portraits.
So:

- **Resume is free.** Stop it anywhere; the next run keeps every finished chunk.
  The cache lives in `<outdir>/<name>.parts/chunks/` and is kept after a render.
- **Swapping a logo is surgical.** Replace `tmp/characters/X.png` and only the
  chunks where X speaks re-render; a byte-identical file hits the cache. The same
  holds for a caption edit, a template tweak, or a change of `--crf`.
- **Chunks are independent**, which is what makes the distributed direction below
  possible: two machines rendering different chunks never coordinate.

Workers run on a rayon pool (`--jobs`, default one per core). Measured on this
box (10 cores): 90 s of video in 10.5 s (≈8.6× realtime) and 450 s in 47 s
(285 fps); the whole 2 h 34 m book projects to ≈16–18 min. Peak RSS is ~400 MB —
the Python renderer it replaces peaked at **12.3 GB** for an 8-second preview,
because it held every caption tile in memory at once. Throughput plateaus around
4 workers: x264 is itself threaded, so more workers only contend.

## Layout

| file | owns |
| --- | --- |
| `template.rs` | the template document: colours, gradient, zones, box model |
| `model.rs` | inputs: the acts manifest and the cue sidecars |
| `text.rs` | the three faces, wrapping, justification, captions, `.srt`/`.vtt` |
| `paint.rs` | the static art: paper, act plate, timeline bar, thumb |
| `sticker.rs` | portraits and the per-frame speech envelope |
| `raster.rs` | the pixel primitives: decode, crop, resize, blit, rotate |
| `ffmpeg.rs` | the ffmpeg/ffprobe wrappers |
| `render.rs` | the chunk plan, cache keys, the parallel loop, assembly |
| `render/layers.rs` | the composition: template layers resolved to painters and sprites |
| `source.rs` | the crate's file list and the digest of its code |
| `distribute.rs` | staging, shipping, per-box build, dispatch |

## The composition is data

`layers` in the template **is** the z-order: an ordered list, drawn first to
last. Omit it and the order it used to be hardcoded in is used, so a template
written before this existed still renders exactly what it did.

| kind | draws |
| --- | --- |
| `paper` | the palette's background gradient |
| `act_plate` | the current act's label and title |
| `bar` | the timeline track and its segments |
| `thumb` | the thumb image on its path along the bar |
| `captions` | the subtitle tiles |
| `portraits` | the speaker's portrait |
| `sprite` | any image, placed in a zone and optionally moving |

A `sprite` names a `zone` (default `illustration`) and an `asset`, relative to
this directory, plus an optional `motion`:

```json
{"kind": "sprite", "zone": "illustration", "asset": "tmp/art.png",
 "motion": {"name": "travel", "period_s": 6}}
```

| motion | moves |
| --- | --- |
| `travel` | left to right across its zone, looping |
| `bob` | up and down, by `amplitude` of the zone's height |
| `spin` | rotates; `direction` is `cw` or `ccw` |

`period_s` defaults to the whole video, so omitting it means one pass from start
to finish. A sprite with no `motion` holds still. The asset is fitted into the
zone without distortion and centred in it.

**A motion is a pure function of the frame index.** Nothing is carried between
frames, so a chunk composes the same alone as it does in sequence, on any box —
which is what lets a render be split, cached and shipped out at all. An effect
that kept state (particles, or an easing that depended on when an element
entered) could not resume mid-chunk; it would have to derive its state from the
frame index. A sprite's bytes are in the chunk key, so swapping the art
re-renders the chunks that show it.

## Rendering across boxes

```sh
plugins/bm-video/target/release/bm-video remote \
  --host 192.168.69.37 --user thang \
  --acts renders/acts.json --slots 2 --worker-jobs 2 \
  --hold-cmd 'pkill -STOP -f bm-agent' --release-cmd 'pkill -CONT -f bm-agent'
```

One **plan owner** (here) holds the source of truth and spreads the work:

1. builds a **stage** — a mirror of this directory holding only what a render
   reads: the template, the manifest, the assets they name, and the published
   mp3s and cue sidecars the manifest lists;
   It also writes `plan.json` into the stage — the timeline as numbers — and
   every worker is handed `--plan`. A box renders the **plan owner's** clock
   instead of probing one of its own, because ffmpeg's container duration
   estimate varies by build: on one chapter here, two builds agreed to the
   decoded byte and disagreed by 38 ms. A timeline off by a single frame is a
   different chunk plan with different chunk keys, so every chunk that box
   rendered would be thrown away on arrival.
2. rsyncs the plugin's **source** (not a binary) and the stage to
   `~/.bm-video/{plugin,stage}` and runs `cargo build --release` there, so each
   box compiles for itself;
3. splits the chunk plan into one contiguous range per worker slot and runs them
   over ssh in parallel (`--slots` per box, `--worker-jobs` threads each);
4. rsyncs each box's finished chunks home into `<outdir>/<name>.parts/chunks/`;
5. runs the ordinary local render, which now finds every chunk cached and only
   assembles.

**Why nothing has to be coordinated.** A chunk is named by a hash of its frame
range and its inputs, and the plan is reproducible from the staged inputs — so
every box computes exactly the names the plan owner did. The key hashes the
speech envelope at the level the portrait tile is actually built from (16 squash
levels), never finer. Boxes run different ffmpeg builds, and those builds report
a level differing in the last digits; quantised finer than the pixels, that noise
alone would re-render chunks whose pixels are identical. There is no queue, no
lock and no reconciliation step; a box that dies simply leaves its chunks
missing, and the next run fills them (or the final local pass renders them
itself). A worker is the same binary with `--from/--count`, and it must be given
the plan owner's clock or it will derive one of its own:

```sh
bm-video --acts renders/acts.json --from 120 --count 60 --outdir … --name … \
  --plan /tmp/bm-video-stage/plan.json
```

`--host local` runs a worker on this machine over `sh` instead of ssh. It is the
same code path without the ssh hop, which is how the dispatch loop is tested.
`--plan-only` prints the plan and each box's exact command without touching
anything.

A box needs: ssh key auth, `rsync`, and a Rust toolchain. The build exports
`$HOME/.cargo/bin` first, because a non-interactive ssh shell does not read
`~/.profile` and so would not find a rustup `cargo` even when it is installed.

`--hold-cmd` / `--release-cmd` run on each box before and after the work: that is
the hook for standing down its normal worker while a plugin-worker has the cores.
The plugin runs whatever command you name and knows nothing about `bm-agent` —
that is what keeps it decoupled.

## The LAN pool

`remote` pushes: the plan owner needs a route to every box, ssh access to it,
and rsync to move inputs out and chunks back. The pool inverts that. The owner
serves; boxes dial in, ask for work and hand chunks back themselves. Nothing is
pushed, no box needs to be reachable, and there is no ssh or rsync in the loop.

```sh
# on the owner (the LAN address of this machine)
bm-video serve --bind 0.0.0.0:8722 --acts renders/acts.json --outdir renders

# on each box, once its binary exists
bm-video join --server 192.168.68.55:8722 --jobs 4
```

A box's first run pulls everything it needs over the same connection — the
plan, the template, the fonts, the portraits and the published chapters (23
files, a few MB) — into `~/.bm-video/{stage,out}`, then asks for batches of
chunks and submits each one. `--dir` puts those somewhere else; `--fetch-source`
also pulls the plugin's own source and rebuilds, so a box can be updated
without rsync.

The owner assembles when the last chunk lands. It renders nothing itself unless
a chunk never arrived.

```
box                                  owner
 │  hello      →  plan + inputs         │
 │  file x N   →  the inputs            │
 │  task       ←  from, count           │   8 chunks, contiguous
 │  (renders)                           │   (needs no inputs from the owner)
 │  submit x N →  chunks                │
 │  task       ←  nothing left          │
```

**A batch is leased, not promised.** An unfinished batch goes back in the pool
after 3 minutes, and only the chunks still missing go back — so a box that dies
mid-batch costs at most that batch, and one that finished but never said so is
not made to redo it. If a box never comes back, the owner's own pass renders
what is missing.

**Getting a binary onto a box is the one bootstrapping step** (the pool cannot
hand out a program that is needed to join it). Ship the source once and build:

```sh
rsync -az --no-perms --exclude target plugins/bm-video/ box:.bm-video/plugin/
ssh box 'export PATH="$HOME/.cargo/bin:$PATH"; cd ~/.bm-video/plugin && cargo build --release'
```

**There is no authentication.** Anyone who can reach the port can read the
inputs and submit chunks. That is the point of a LAN pool and the reason to
bind it deliberately: `--bind 127.0.0.1:8722` for one machine, an address on the
private network otherwise — never a public one.

`remote` remains the mode to reach a box the owner cannot serve to, or one
behind a NAT it cannot dial out from.

## Is the box on the same code?

Two renderers on different code write different pixels under an identical chunk
name, so the difference would arrive as a silently wrong video rather than as a
failed run. The plan therefore travels with a fingerprint of the code, and each
box proves itself against it before any pixels are rendered.

```sh
bm-video digest                 # 64 hex digits, this source's fingerprint
```

The digest covers the files that decide the binary — everything under `src/`,
plus `Cargo.toml` and `Cargo.lock` — as each one's relative path and bytes, in
sorted order. Docs are not part of it, so editing the README does not make every
box look stale, and neither the platform nor where the tree sits enters into it:
an arm64 owner and an x86 box agree.

- `serve` prints it in the banner and sends it with the job.
- `join` asks the binary it is about to render with, and on a mismatch refuses:
  `this box is on source 9ec061585165 but the pool is on b0d6895d540d — re-join
  with --fetch-source to update it, or pass --allow-stale to render anyway`.
  With `--fetch-source` it fetches the pool's source, builds it, checks the
  result, and **renders with the binary it just built** — one command updates a
  box that has drifted.
- `remote` ships the source as a **mirror** (`rsync --delete`, `target/`
  preserved), so a file deleted here cannot linger on the box and make its digest
  differ for no real reason. After the box's `cargo build --release` the owner
  asks the box what it hashes to, and stops the run on a difference — before
  anything is dispatched.

The check is over the source a binary is built from, and both paths ask the box
right after it builds, so for an updated box it is the tree the running binary
was compiled in.

## Limitations

- `--remote-root` cannot contain spaces (rsync's remote-path quoting), and every
  template path must be relative to this directory, because the stage mirrors it.
- A portrait swap leaves the previous chunks in place (the cache is keyed, not
  garbage-collected), so `rm -rf <outdir>/<name>.parts` reclaims that space.
- A `sprite` is on screen for every frame, so changing its art re-renders the
  whole video. That is unlike a portrait, which is keyed to only the chunks its
  speaker talks in.
- A box's chunks are pulled only after all its workers succeed; a failure there
  is reported, not retried. In the pool, a failed batch simply comes back when
  its lease runs out.
- A joining box re-fetches every input on every run rather than comparing sizes,
  so a re-join moves the whole stage again (a few MB for one chapter).
- The digest is over the source, not over the artifact: it says nothing about a
  binary someone built by hand and then left behind while the source moved on.
  Both paths land the source and build in the same run, so neither can hit that.
