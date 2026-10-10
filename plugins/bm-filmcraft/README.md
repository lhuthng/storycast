# bm-filmcraft

Turns a Storycast book's acts into a **FilmCraft project** — the same chapters,
captions and act titles, as a timeline you can open in
[FilmCraft](https://github.com/storytold/filmcraft) and keep working on — and can
then export through FilmCraft's own H.264 encoder.

It is an **out-of-tree plugin**. It is not a member of the Rust workspace under
`rust/`, nothing in the pipeline depends on it, and it depends on nothing in the
pipeline — it reads files and drives another program. `cargo build --workspace`
and `make test` in `rust/` do not build, test or lint it. It has its own
workspace root here, its own `Cargo.lock` and its own `target/`.

It does not replace [bm-video](../bm-video/README.md), which renders the book's
*pictures* (paper ground, act plate, timeline bar, portraits) with its own
compositor and ffmpeg — but it does not import that render either. The look is
composed **inside FilmCraft**, from the same template `bm-video` renders from:
the paper, the act plate, the bar, the playing act's highlight, the thumb that
turns and moves along it, the speaker portraits that squash on the syllable and
the caption style are graphics FilmCraft builds and its own encoder draws — the
thumb and the portraits set moving with keyframes on their own motion effects. An import of a
pre-rendered mp4 would be a second renderer's output pasted under the edit; this
is one project, one renderer, one clock.

## What it needs

- `filmcraft-cli` **0.4.0**. Either the release build for your platform
  (`filmcraft-cli-<version>-<platform>.zip` on the
  [releases page](https://github.com/storytold/filmcraft/releases) — a signed,
  notarized binary, no Rust toolchain needed), or
  `cargo build --release -p filmcraft-cli` in a FilmCraft checkout. Pass it with
  `--filmcraft-cli <path>`; the default is `filmcraft-cli` on `PATH`.
- The book's published output: `output/Ch.N - Title.mp3` and its
  `Ch.N - Title.cues.json` sidecar, which the pipeline already writes.
- `ffmpeg` on `PATH` when the template has speaker portraits: the speech envelope
  behind each badge's reaction is measured from the mp3 with it.
- The render template: `tools/video-template.json` by default, or `--template
  <path>`. Canvas, frame rate, palette, zones, type and the `layers` list all come
  from it — it is the same file `bm-video` renders from, so the two cannot drift
  apart by hand, and a `sprite` it places is placed and keyed here too.

Nothing else: no fonts to install, and the art a template names — a portrait, the
thumb's picture, a sprite — is read from the repo beside it. FilmCraft brings its
own codecs.

## MCP, or the CLI?

FilmCraft does ship an MCP server: `filmcraft-cli mcp` (the `automation` crate, the official
`rmcp` SDK, stdio), in two modes — headless (`--demo`, or `--project p.fcproj`) against an
in-process engine, and a bridge (`--bridge 127.0.0.1:9876`) against a running app started as
`filmcraft --control 9876`. Its `command_run`, `command_batch`, `media_import` and
`sequence_inspect` tools dispatch the **same engine commands** this plugin's script does — every
FilmCraft front end does (see FilmCraft's `docs/agents.md`).

This plugin drives `filmcraft-cli exec` and `filmcraft-cli run` instead. The edit is a known batch
of commands, so it wants a one-shot process that exits with a status rather than a server an agent
has to keep alive and hold a session in. `run` takes the same `{id, params}` lines a
`command_batch` would and stops at the first failure, so the build is legible — `<name>.script.jsonl`
is every step it ran, in order — and re-runnable onto a project holding the same imports, without a
client library. It is a record, not a rebuild: the steps are positional, so running it into the
project it already built places the sound a second time and then fails on the act plate (`V1 is not
free here`). `--force` is what rebuilds.

Reach for the MCP server instead when someone is working the project **interactively** with an
agent: `render_frame` to look at a frame, `ui_click` / `ui_type` to drive the app's own UI,
`edit.undo` when a step was wrong. Both routes dispatch the same commands into the same `.fcproj`,
so an agent can pick up a project this plugin built and keep going.

## Build and run

```sh
cd plugins/bm-filmcraft
make release                     # optimized binary
make test                        # cargo test + clippy -D warnings (this plugin only)

./target/release/bm-filmcraft \
  --acts ../../renders/acts.json \
  --out ../../renders/acts-01-04.fcproj \
  --export ../../renders/acts-01-04.mp4 \
  --filmcraft-cli /path/to/filmcraft-cli
```

Run it from the repository root: `--workspace` (the book's `output/`) and every
path default resolve from the working directory.

Useful flags: `--plan` (print the edit and stop, without FilmCraft), `--preview
SECS` (export only the first N seconds — a timing check), `--chapter-gap S`,
`--template PATH`, `--canvas WxH` and `--fps` (override the template's, for a
different frame), `--no-subs` (no burned captions; the `.srt` is still written),
`--force` (overwrite an existing project).

## What it builds

| Storycast | becomes |
| --- | --- |
| `output/Ch.N - ….mp3` | an imported item, placed on **A1** at its chapter's start |
| the chapter gap | silence between the clips — a real gap, not a cut |
| `….cues.json` | one `.srt` for the whole book, imported to a caption track named `Storycast`, and written beside the project |
| the template's paper | a full-frame rectangle, the palette's background |
| each act | the plate: the label (`Hồi 1`) over the title, both in the template's fonts, sizes, inks and zone, up for the act's whole window, + a chapter marker at its first chapter |
| the act's length | a segment on the bar, in the timeline zone, at the template's thickness and colour |
| the act that is playing | the segment *raised* — `active_y_scale` of the track's height — in `timeline_segment_active`, up for that act's window and no longer |
| where playback has reached | the thumb: the template's own illustration at the disc's diameter, turning a whole revolution every `spin_s` in the direction it names, travelling the bar's left edge to its right over the whole book |
| the subtitle block | the caption track's style: font, size, ink, the caption plate's colour, alignment and margin |
| the speaker a cue names | a portrait: the template's art for that name (case-insensitively), `anonymous.png` for a name it does not map, none for the narrator — sized off the timeline thumb's own ratio, up for that cue's window and no longer |
| how loud the chapter is | the badge's reaction: measured per video frame with ffmpeg, its own percentiles as the floor and ceiling, instant on the syllable and settling after — keyframed into `scale` and `scale_width` |
| a `layers` entry of kind `sprite` | the template's own picture, in its zone: fitted and centred exactly as `bm-video` fits it, on its own track above the portraits, with the `motion` it is given keyed onto it |
| — | `<name>.script.jsonl`: every command it ran, in order, as `filmcraft-cli run` takes them — the build, in a form a person or an agent can read and re-run (`--force`, not this file, is what rebuilds) |
| — | `<name>.anim.jsonl`: the second pass — the thumb's and the badges' keyframes, which can only be written once `graphics.newShape` or `timeline.place` has told us their clip ids |

`--plan` prints the edit; a build prints the same table from the lengths
FilmCraft reported, so the two can be compared.

## A sprite's motion

Any picture the template's `layers` place is a `sprite`, and one can be animated
by its own **tracks**: a segment per property, eased from one value to another
over a length of time, one-shot or repeating. A picture that should ride a sine
until the book ends, swaying and fading in at the start and out at the end, is
the whole of this:

```json
{"kind": "sprite", "zone": "illustration", "asset": "tmp/art.png",
 "motion": {"period_s": 2.0, "tracks": [
   {"prop": "y", "from": -0.05, "to": 0.05, "dur_s": 1.0, "ease": "in-out-sin",
    "repeat": 0, "pingpong": true},
   {"prop": "rotate", "from": -8, "to": 8, "dur_s": 1.0, "ease": "in-out-sin",
    "repeat": 0, "pingpong": true},
   {"prop": "opacity", "from": 0, "to": 1, "dur_s": 0.5, "ease": "out"},
   {"prop": "opacity", "from": 1, "to": 0, "dur_s": 0.5, "ease": "in", "align": "end"}
 ]}}
```

`prop` is `x`, `y` (fractions of the zone's width and height), `scale` (a
multiple of the fitted size), `rotate` (degrees) or `opacity` (0..1); `ease` is
`linear`, `in` / `out` / `in-out`, `in-cubic` …, `in-sin` / `out-sin` /
`in-out-sin` (`-squared` is `-quad`); `repeat` is how many passes, `0` meaning to
the end of the book; `pingpong` reverses on each pass, so a half-period segment
eased `in-out-sin` is a whole sine; `align` is where a one-shot segment begins.
Two tracks may name the same property — they are then segments in time order —
which is how the fade in and the fade out above are written.

FilmCraft tweens two keys in a straight line, so each track is **decimated** into
the frames that hold its eased shape to within half a pixel, half a degree and
half a percent: a 2 s sine bob costs about 13 keys a cycle, not 48. Those are fake
keys on a curve, but each one is *the curve's* value at its frame, so the export
is the template's motion rather than an approximation a person has to correct. A
property no track moves is set once, and never keyed at all. The clip's
`motion.anchor` is left alone, so a sprite turns about its own centre rather than
the frame's: on a ±8° sway the ink's centroid moved 0.9 px across the swing,
where an anchor at the frame's centre would have moved it ±89 px. `bm-video` reads the
same tracks and evaluates them exactly, so the two renderers agree to within the
half pixel. What it does not share is the general case: its sprite painter
evaluates turning and resizing from a pose table one `period_s` long, so a
non-repeating `scale` or `rotate` has to be given a `period_s` there.

## Two clocks, and which one wins

The cue sheet knows when the last *cue* ends; the mp3 is the mix that cue sheet
describes, and it runs past that — 1.5 s on chapter 1 here (411.75 s of cue
sheet, 413.27 s of mp3, because the mixer wrote a tail the last cue does not
cover). `bm-video` probes the audio for the same reason.

So the plugin builds the edit in two passes: `file.import` first, then
`project.inspect` for the length FilmCraft decoded each item at, and the
chapters are laid out on **that** clock, with the gap added between them. Laying
chapter 2 down on the cue sheet's figure instead would overwrite the end of
chapter 1 — its last second of delivered speech — whenever the two disagree.

Captions are written after that layout, so they carry the same offsets. FilmCraft
snaps caption times to frame boundaries, so a caption can land up to half a frame
from the cue's exact instant.

Every clip edge the edit sends goes on that same frame grid, for a harder reason:
FilmCraft rounds a clip's own **end up** to a whole frame, so when two clips butt
up on one track — the plate's acts, or a badge following another on its lane — and
the shared instant falls between two frames, the second is refused (`V11 is not
free here`). A 30-chapter book's act starts do not land on frames, so the plate
and the badges are placed at whole frames off `fps`; a test builds a book whose
boundary is between frames and holds the two acts to the same one.

## How to check it

```sh
filmcraft-cli probe renders/acts-01-04.mp4          # H.264/AAC, length, frame rate
filmcraft-cli --project renders/acts-01-04.fcproj inspect sequence
filmcraft-cli --project renders/acts-01-04.fcproj exec captions.list
filmcraft-cli --project renders/acts-01-04.fcproj exec graphics.list '{"clip": N}'
```

`graphics.list` shows a graphic clip's layers — where the plate landed, what it
was told to be. The look's own tracks are `V1` paper, `V2` the bar's groove, then
one segment track per act, then one for each act's raised highlight, then the
plate's label and title, then the thumb, and the portraits above that: a track
holds one clip at a time, and the paper, the groove and every segment run the
whole book.

The thumb and the portraits are animated in a **second pass**. A timed
`graphics.set` or `effects.setParam` replaces a property's value unless the
property is already animated, and `effects.addKeyframe` wants the clip's id,
which only the reply to `graphics.newShape` or `timeline.place` carries — so the
build runs, the replies are read, and the keys are written against the project
the build just made. In the project the thumb's `motion.position` then holds two
keys, `0` at the bar's left edge and the book's last tick at its right, linear
between: exactly the line `bm-video` moves it along. Its `motion.rotation` holds
three — `0`, one turn after `spin_s`, and the book's last tick — so the angle is
a straight line too, where `bm-video` steps a table of pre-rotated frames.

A badge's reaction is keyed on **every frame that moves it by more than a
fiftieth**, peaks included: the attack is exact, the release is a straight line
between keys instead of a curve, and a book whose speakers are quiet between
syllables carries a fraction of the frames rather than all of them. The badge's
two axes move apart because `uniform_scale` is turned off — FilmCraft's `scale`
is then the height and `scale_width` the width, each a percentage of the art's
own pixels, which is what the template's `react` asks for.

Opening the project in the desktop app is the positional argument, not `open -a`:
`filmcraft <name>.fcproj`. FilmCraft declares no document type for `.fcproj`, so a
Finder double-click or `open -a FilmCraft <project>` only launches the app — it
comes up on whatever its startup setting says, not on your project. Add
`--control 9876` and the app runs the same control server the CLI and MCP use, so
the project can be inspected (`filmcraft-cli --bridge 127.0.0.1:9876 inspect
sequence`) while it is open. Every verb the plugin drives takes `--bridge` too,
so a script it wrote — including the animation pass — can be replayed into the
open app, and more steps added there one at a time. Use `--no-recover` when a previous session left
unsaved changes you do not want restored over it.

The inspection is the one to look at: each chapter's clip should start at the
previous one's end plus the gap, with no clip shorter than the audio it holds,
and no two clips overlapping. `filmcraft-cli --project … render --seconds S --out
frame.png` renders one frame at any second if you want to look at the titles and
captions.

## Limitations

- **A sprite's motion is keyed, not evaluated.** FilmCraft draws a straight line
  between two keys, so a track is decimated to the points that hold its shape
  within half a pixel, half a degree or half a percent. Each key is *on* the
  curve — every one of a demo sprite's 90 position keys measured within a
  thousandth of a pixel of its sine — but a motion finer than the tolerance (a
  0.4 px sway, a 0.3° wobble) is quantised to it, and between keys the curve is a
  chord.
- **The thumb's art is not round-clipped.** `bm-video` masks it to a disc before
  drawing; this plugin draws the file as it is. Today's `thumb-single.png` is
  already a disc — 78% of its box is opaque, against π/4, with 14 anti-aliased
  edge pixels outside the circle and none missing inside — so the mask is a no-op
  and the two agree exactly. Art that is square, with a background in its
  corners, would draw square here. A template with no art at all still gets the
  disc, ring and ink `bm-video` falls back to.
- **A portrait must be tight to its figure.** `bm-video` crops the art to its
  alpha bounding box before sizing it; this plugin takes the file's own pixels,
  because that is what FilmCraft scales. Every portrait in `tmp/characters/`
  (and the thumb's own art) is tight, so the two agree exactly today; art with a
  transparent margin would draw smaller here.
- **The badge's reaction needs ffmpeg.** The envelope is one `ffmpeg` pass per
  chapter over the published mp3, so the plugin needs `ffmpeg` on `PATH` when the
  template has portraits, and a build with them takes as long as those passes do.
- **The paper is a solid, not a gradient.** FilmCraft has no gradient fill
  command, so the palette's background resolves to the middle stop of its CSS
  gradient — within a few levels of the wash bm-video paints.
- **The plate's fonts must be installed.** FilmCraft renders in system fonts;
  the template's `font_files` live in the repo, not in the system. The plugin
  names every missing family at the end of a build and the file to install it
  from (`cp 'tmp/FC-Opulent-Brush.otf' ~/Library/Fonts/`); without that, the
  plate and the captions render in a fallback face at the right size.
- **A build is one shot.** It creates a new project and refuses to overwrite an
  existing one without `--force`, so a rebuild re-imports every chapter. There is
  no chunk cache here — FilmCraft's own render previews are the caching story,
  and they live in the project.
- **H.264 only.** The export path sets `format: h264`; FilmCraft's other delivery
  formats are not wired up to a flag yet.
- **FilmCraft must be installed.** This plugin cannot be part of `make test` in
  `rust/`, and its checks are run by hand, like the pipeline's other live-backend
  checks.
- **No MCP client.** The plugin talks to `filmcraft-cli`, so it needs the CLI on
  disk; the MCP server is a separate door into the same engine, not a
  requirement here (see above).
- **One act title per act.** The plate reads the manifest's act title; the
  per-chapter headline the merged audio already speaks is left to the captions.
