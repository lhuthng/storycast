# What a pack needs

**In one line:** the shopping list. Every sound each pack is supposed to have,
and which of them are still missing.

*You can stop reading after §The two rules. The rest is a per-pack inventory.*

Read this when you want to know **what a pack should contain**, not how to
record it — that is [SOUND.md](SOUND.md) — and not how packs compose, which is
[ASSETS.md](ASSETS.md). To finish one that is already half-built, follow
[COMPLETING-A-PACK.md](COMPLETING-A-PACK.md).

Two kinds of pack, and each holds one kind of thing:

* **an sfx pack** — **places** (a bed that runs under a whole scene) and
  **moments** (a one-shot that lands on a beat). **No music, ever.**
* **a music pack** — **tracks** only. No places, no moments.

Every sound needs **two takes**, because the picker rolls once to choose the
sound and again among that sound's files; one file plays the same recording every
time the book asks for it. Tags are what the picker matches on, so they are part
of the spec, not decoration — a sound whose tags nothing names is silent, with no
error anywhere.

**A prompt is a cause, not a gesture.** Two things earn their place in a row: the
action *and its direction* (`a wooden comb pulled down through long hair from the
crown to the ends`, not `a wooden comb drawn through long hair`), and the
material's *response* — the teeth catching and releasing, the wood splitting, the
string still ringing. A gesture with no direction comes back as the same sound
played the wrong way, and a note about technique (`never hurried`, `unhurried`)
is heard as neither one.

## `common` — sfx · 11 places, 30 moments

**Places — 60 s, seamless, 2 takes each:**

| place | tags | prompt |
| --- | --- | --- |
| `cave-drip` | `cave` `drip` `dark` | `cave interior, occasional water drip into a pool, deep stone reverb, still air, no hum, no voices, no music, seamless, 60 seconds` |
| `day` | `day` `calm` | `open countryside in daylight, light wind through grass, distant birdsong, no voices, no music, seamless, 60 seconds` |
| `fire-crackle` | `fire` `indoor` `warm` | `close small hearth fire, dry wood crackle and settle, ember hiss, warm room tone, no voices, no music, seamless, 60 seconds` |
| `forest` | `forest` `birds` `day` | `temperate woodland in daylight, layered birdsong, leaves moving, no voices, no music, seamless, 60 seconds` |
| `market-crowd` | `market` `crowd` `street` `day` | `distant day market crowd, indistinct chatter, cart wheels on stone, no intelligible words, no music, seamless, 60 seconds` |
| `night` | `night` | `summer night outdoors, a field of crickets and distant insects, still air, nothing else, no drone, no voices, no music, seamless, 60 seconds` |
| `rain` | `rain` `calm` | `steady rain on a wooden roof and wet ground, no thunder, no voices, no music, seamless, 60 seconds` |
| `river` | `river` `water` | `a wide river running over stones, water moving continuously, no voices, no music, seamless, 60 seconds` |
| `snow` | `snow` `winter` `cold` | `snow falling through still air onto open snow, muffled winter air, a soft gust now and then, no howl, no drone, no voices, no music, seamless, 60 seconds` |
| `storm` | `rain` `storm` `tense` | `heavy rain on tiled roofs, rolling thunder some distance off, gusting wind, no voices, no music, seamless, 60 seconds` |
| `wind` | `mountain` `wind` `cold` `winter` | `high mountain wind across bare rock, thin and cold, no voices, no music, seamless, 60 seconds` |

**Moments — short one-shots, 2 takes each where they hold up:**

| moment | tags | length | prompt |
| --- | --- | --- | --- |
| `bark` | `bark` `dog` `woof` `guard` | 2.0 s | `a large dog barking twice, close, outdoors` |
| `bell` | `bell` `chime` `gong` `ritual` `temple` | 2.0 s | `a small temple bell struck once, close, with a short ring` |
| `body-thud` | `thud` `body` `fall` `collapse` | 2.0 s | `a body dropping onto a wooden floor, one thud, no voice` |
| `boiling-water` | `water` `boiling` `pot` `cooking` | 51 s loop | `a pot of water at a full boil, lid off, steady bubbling, seamless, 51 seconds` |
| `broom-sweep` | `sweep` `broom` `chores` `cleaning` | 2.0 s | `a straw broom on a stone floor, two short strokes` |
| `burp` | `burp` `belch` `stomach` | 2.0 s | `a single low burp, close, human, no voice` |
| `cloth-flutter` | `cloth` `flutter` `sleeve` `cloak` `robe` | 2.0 s | `heavy cloth lifted and settling, one flutter` — **recorded** |
| `coin` | `coin` `money` `metal` `clink` | 2.0 s | `three coins dropped and settling on a wooden surface` |
| `cooking` | `cooking` `food` `wok` `fire` | 22 s loop | `a wok over a fire, oil and food turning, spatula on metal, seamless, 22 seconds` |
| `crowd-cheer` | `cheer` `crowd` `shout` `celebrate` | 4.4 s | `a crowd cheering and whistling, distant, no words, no music` |
| `door-knock` | `knock` `door` `kowtow` `rap` | 2.0 s | `three knuckles rapping a wooden door, close` |
| `flyby` | `whoosh` `flyby` `pass` `movement` | 2.0 s | `a large bird's wings passing close overhead, one pass` |
| `food-prep` | `prep` `kitchen` `chop` `board` | 25 s loop | `a knife chopping vegetables on a wooden board, steady rhythm, seamless, 25 seconds` |
| `footstep-forest` | `steps` `leaves` `trail` `forest` | 51 s loop | `slow footsteps on a forest trail, dry leaves and dirt, one walker, seamless, 51 seconds` |
| `footstep-stone` | `steps` `stone` `walk` `street` | 7 s loop | `slow footsteps on a stone street, hard soles, one walker, seamless, 7 seconds` |
| `footstep-wood` | `steps` `wood` `floor` `indoor` | 17 s loop | `slow footsteps on a wooden floor indoors, one walker, seamless, 17 seconds` |
| `gulp` | `gulp` `swallow` `drink` `throat` | 2.0 s | `in a quiet room someone takes a mouthful and swallows it, one close gulp, the throat closing, wet and dry at once, no voice` — **recorded** |
| `hoofbeats` | `gallop` `hoof` `horse` `march` `ride` | 37 s loop | `a horse at a walking pace on packed earth, hooves and harness, seamless, 37 seconds` |
| `howl` | `howl` `wolf` `dog` `moon` | 7.7 s | `a single wolf howl, distant, open landscape` |
| `metal-hit` | `metal` `clang` `strike` `hit` | 2.0 s | `a metal object struck once, clang with a quick decay` |
| `page-turn` | `paper` `page` `book` `flip` | 2.0 s | `one paper page turned, close, dry` |
| `pour` | `pour` `water` `liquid` `drink` | 3.5 s | `water poured from a jug into a cup, liquid rising` |
| `rock-break` | `rock` `stone` `break` `crash` `shatter` | 3.2 s | `a large rock is struck and splits open, pieces of stone scatter across the ground` — **recorded** |
| `slap` | `slap` `cheek` `face` `smack` | 2.0 s | `an open palm slap on skin, close, one hit` |
| `snore` | `snore` `sleep` `snoring` | 4.1 s | `one slow snore cycle, sleeping breath, close` |
| `stomach-growl` | `growl` `stomach` `hungry` `belly` | 2.8 s | `a hungry stomach growling twice, close, no voice` |
| `swoosh` | `whoosh` `swing` `movement` | 2.0 s | `a short cloth whoosh, one swing past the microphone` |
| `tree-hit` | `wood` `tree` `crash` `hit` | 2.0 s | `a heavy blow landing on a tree trunk, wood and leaves shaking` |
| `wood-break` | `wood` `break` `crash` `splinter` | 2.0 s | `a plank snapping and splintering, one break` |
| `wood-chop` | `wood` `chop` `axe` | 2.0 s | `an axe blade biting into a dry log, one chop, the wood splitting and the handle thudding, close` |

## `court-mystery` — sfx · 4 places, 9 moments, 15 tracks in the music pack

**Places — 60 s, seamless, 2 takes each:**

| place | tags | prompt |
| --- | --- | --- |
| `palace-corridor` | `marble` `indoor` `hall` `empty` `long` | `long marble corridor with paper screens, one footstep two rooms away every twenty seconds, deep dry reverb, no voices, no music, seamless, 60 seconds` |
| `palace-garden` | `garden` `outdoor` `night` `water` `insects` `calm` | `walled garden at night, water trickling, crickets and insects, a stone path, no voices, no music, seamless, 60 seconds` |
| `pleasure-quarter-night` | `pleasure` `street` `night` `market` `lanterns` `crowd` `canal` | `narrow lantern-lit street at night, a canal, a low indistinct crowd, wooden wheels on stone, no intelligible words, no music, seamless, 60 seconds` |
| `rear-palace-courtyard` | `courtyard` `outdoor` `day` `open` `distant` | `open stone courtyard in daylight, still air, a heavy door closing somewhere out of sight, distant birds, no voices, no music, seamless, 60 seconds` |

**Moments — 2 takes each:**

| moment | tags | length | prompt |
| --- | --- | --- | --- |
| `comb-through-hair` | `comb` `hair` `brushing` `grooming` `dressing` | 2.5 s | `a wooden comb pulled down through long hair from the crown to the ends, the teeth catching and releasing, one even pass` |
| `curtain-sweep` | `curtain` `drape` `screen` `drawn` `passing` `fabric` | 2.0 s | `a heavy curtain pulled aside along its rail, the fabric sliding and gathering to one side` |
| `gate-shut` | `gate` `door` `closing` `slam` `latch` `shutting` `barred` | 2.0 s | `a heavy timber gate: the bar lifting, the swing, the slam, then the ring-out` |
| `hairpin-set` | `hairpin` `pin` `ornament` `hair` `coiffure` `dressing` `sticking` | 2.0 s | `a slender wooden hair stick pushed into a bun, one small dry click` |
| `kneel-kimono` | `kneeling` `knees` `bowing` `prostrating` `mat` `fabric` `court` | 2.0 s | `heavy fabric and knees settling on a rush mat, settled not dropped` |
| `lattice-close` | `lattice` `window` `shutter` `shutting` `paper` `sliding` | 2.0 s | `a paper lattice window pushed shut, light wood sliding in a frame` |
| `porridge-spill` | `spill` `splash` `liquid` `porridge` `food` `dropped` `wet` | 2.0 s | `thick porridge spilling and spreading, wet slap, no dishes` |
| `silk-rustle` | `silk` `rustle` `fabric` `robes` `sleeves` `walking` `turning` `cloth` | 2.0 s | `heavy silk robes over a cedar floor, one turn and one step` |
| `tray-drop` | `crash` `tray` `dropping` `porcelain` `smash` `shattering` `falling` `floor` | 2.2 s | `a lacquer tray and its bowls hitting a stone floor, one impact then debris settling` |

**Score — 15 tracks.** Prompts, the shared suffix and the loop cut are in
[court-mystery/TRACKS.md](../assets/_extends/court-mystery/TRACKS.md): one prompt
a track, `court-hall`, `inquiry-grind`, `wicked-plan`, `illness-wane`,
`cedar-corridor`, `grief-quiet`, `wry-pipa` and the rest. They go in the music
pack, not here — an sfx pack holds no music.

## `craft` — sfx · 1 place, 9 moments

**Place — 60 s, seamless, 2 takes:**

| place | tags | prompt |
| --- | --- | --- |
| `workshop-interior` | `workshop` `indoor` `warm` `room` | `a shut apothecary workshop, low interior room tone, a pestle and a drawer somewhere two rooms away, dust and dry wood, no voices, no music, seamless, 60 seconds` |

**Moments — 2 takes each:**

| moment | tags | length | prompt |
| --- | --- | --- | --- |
| `bottle-unstopper` | `bottle` `stopper` `cork` `phial` `unguent` `ointment` `dispensing` | 2.0 s | `a cork, then a ceramic stopper: twist and lift, no pop` |
| `bowl-set-down` | `bowl` `dish` `saucer` `cup` `porcelain` `ceramic` `table` `meal` | 2.0 s | `a porcelain bowl set down on a wooden counter, set rather than dropped` |
| `brazier-glow` | `brazier` `charcoal` `ember` `hearth` `fire` `stove` `warm` | 45 s loop | `a bronze brazier of charcoal settling, low steady flame and ember pops, no events, seamless, 45 seconds` |
| `cabinet-shut` | `cabinet` `cupboard` `chest` `latch` `lock` `shutting` | 2.0 s | `two small brass latches on a cabinet, one after the other, then the door shutting` |
| `drawer-slide` | `drawer` `sliding` `opening` `reaching` `searching` `rummaging` | 2.0 s | `a cedar drawer pulled open hard and stopped dead, wood on wood` |
| `herb-rustle` | `herb` `leaves` `dried` `foliage` `rustling` `gathering` `plant` | 2.0 s | `a handful of dry leaves and stems poured and settling, dry papery rustle, not a shake` |
| `ladle-stir` | `ladle` `spoon` `stirring` `pot` `kettle` `soup` `decoction` `brewing` | 4.0 s | `a metal ladle circling the inside of a cooking pot, one full turn, the bowl scraping the wall of the pot` |
| `mortar-grind` | `mortar` `pestle` `grinding` `grind` `powder` `pharmacy` `medicine` `herbs` `apothecary` | 2.0 s | `a granite mortar and pestle, one full turn grinding dry herbs, the stone releasing at the end` |
| `paper-powder` | `paper` `powder` `wrapping` `folding` `packet` `dose` `prescription` | 2.0 s | `fine powder tipped onto oiled paper, then the paper folded and pressed with a thumb` |

## `weapons` — sfx · 2 places, 11 moments

**Places, 2 takes each.** `sword-fight` is a spot bed — one clash at the head of
a window — and `sword-war` has to hold a whole battle:

| place | tags | length | prompt |
| --- | --- | --- | --- |
| `sword-fight` | `battle` `sword` | 4 s | `a short sparse blade exchange, two or three clashes close together, no voices, no music` |
| `sword-war` | `army` `battle` `melee` `siege` `sword` `war` | 60 s seamless | `a distant massed battle, metal and shouting far away, no intelligible words, no music, seamless, 60 seconds` |

**Moments — 2 takes each:**

| moment | tags | length | prompt |
| --- | --- | --- | --- |
| `air-swing` | `blade` `movement` `spear` `swing` `swoosh` `thrust` `whoosh` | 2.0 s | `a blade cutting air, one fast swing past the microphone` |
| `arrow-twang` | `arrow` `bow` `shoot` `twang` | 2.0 s | `a bowstring released, a dry twang as the arrow leaves, the string still ringing, one shot` |
| `blood-spatter` | `blood` `gore` `wound` | 2.0 s | `an edge meeting flesh, one wet impact, then droplets scattering outward, close, no voice` |
| `blunt-impact` | `blow` `blunt` `body` `collapse` `fall` `impact` `punch` `thud` | 2.0 s | `a heavy blunt blow landing on a body, one hit` |
| `bone-crush` | `crush` `bone` `crunch` `gore` | 2.0 s | `a bone snapping under a heavy blow, one dry crack then grinding, no voice` |
| `explosion` | `explosion` `blast` `battle` | 3.9 s | `a battle explosion, sharp report with rubble and a long tail` |
| `metal-clash` | `blade` `clang` `clash` `metal` `parry` `strike` `sword` | 2.0 s | `two steel blades meeting, one hard clash with a ring-off` |
| `metal-punch` | `punch` `impact` `metal` `blow` | 3.4 s | `a heavy metal object struck hard in a stone hall, ringing metal with a long decay` |
| `sword-slash` | `sword` `slash` `blade` `battle` | 2.0 s | `a blade cutting through air, one fast slash, the edge hissing as it passes the microphone` |
| `underwater-explosion` | `explosion` `water` `muffled` `blast` | 6.0 s | `an underwater explosion, deep muffled boom, churning bubbles and water rush` |
| `whip-crack` | `crack` `lash` `snap` `strike` `whip` | 2.0 s | `a leather whip cracking once, close, no voice` |

## `magic` — sfx · 0 places, 6 moments

No places: a spell is named by the script and lands as a moment. The impact a
spell makes is not a physical one, which is why `metal-hit`, `rock-break` and
`bone-crush` live in `common`.

| moment | tags | length | prompt |
| --- | --- | --- | --- |
| `curse-qi` | `curse` `dark` `hex` `magic` `spell` | 4.6 s | `dark energy gathering in a still room, a low dissonant hum rising and thickening, no melody, no music` |
| `fire-spell` | `fire` `spell` `magic` `flame` | 4.3 s | `a fire spell cast, a whoosh of igniting flame and a low roar, no music` |
| `ice-spell` | `cold` `freeze` `frost` `ice` `magic` `spell` | 2.1 s | `frost magic, crystalline crackle spreading over stone, icy shards forming, no music` |
| `light-spell` | `light` `spell` `magic` `heal` | 7.6 s | `radiant magical energy gathering, bright sustained shimmer, rising then dissolving, cinematic, no music` |
| `lightning-spell` | `lightning` `thunder` `spell` `magic` | 2.0 s | `an arcane lightning strike, sharp electrical crack with a brief magical tail, no music` |
| `water-spell` | `water` `spell` `magic` | 2.0 s | `a water spell, a surge of conjured water and a hiss, no music` |

## `chinese-music` — music · 10 tracks, 2 takes a mood

Nothing but music. One track answers one mood token, and the tags are the mood's
own words — the script emits the token and its tags are what reach the pool, so a
track answering none of them is never heard.

| track | tags | prompt |
| --- | --- | --- |
| `generic-energetic` | `energetic` `upbeat` | `orchestral theme, driving drums and erhu over massed strings, rising heroic lead, instrumental, no vocals, no lyrics, 2 minutes` |
| `generic-energetic` *(2nd)* | `energetic` `upbeat` | `the same scale with dizi and pipa, faster and brighter, instrumental, no vocals, no lyrics, 2 minutes` |
| `tavern` | `indoor` `warm` | `warm indoor room, guzheng and low strings, unhurried, candlelit, instrumental, no vocals, no lyrics, 2 minutes` |
| `tavern` *(2nd)* | `indoor` `warm` | `the same room at a quieter hour, pipa and soft flute, fewer instruments, instrumental, no vocals, no lyrics, 2 minutes` |
| `market` | `market` `busy` | `busy market street, pipa and clappers, quick light rhythm, instrumental, no vocals, no lyrics, 2 minutes` |
| `market` *(2nd)* | `market` `busy` | `the same street later in the day, xiao and plucked strings over busier percussion, instrumental, no vocals, no lyrics, 2 minutes` |
| `intense-battle` | `battle` `intense` | `close combat, war drums and low brass, urgent, no heroics, instrumental, no vocals, no lyrics, 2 minutes` |
| `intense-battle` *(2nd)* | `battle` `intense` | `a longer pitched battle, taiko and massed strings, surging and receding, instrumental, no vocals, no lyrics, 2 minutes` |
| `sad` | `sad` `sorrow` | `solo erhu, slow grief, sparse, one line at a time, instrumental, no vocals, no lyrics, 2 minutes` |
| `sad` *(2nd)* | `sad` `sorrow` | `the same grief with a low string pad and a distant bell, instrumental, no vocals, no lyrics, 2 minutes` |
| `soft-cute` | `romantic` | `tender guzheng and soft flute, gentle and close, instrumental, no vocals, no lyrics, 2 minutes` |
| `soft-cute` *(2nd)* | `romantic` | `the same tenderness with celesta and a string pad, instrumental, no vocals, no lyrics, 2 minutes` |
| `playful` | `playful` `light` | `light pipa, comic bounce, plucked throughout, instrumental, no vocals, no lyrics, 2 minutes` |
| `playful` *(2nd)* | `playful` `light` | `the same mood with marimba and dizi, quicker, instrumental, no vocals, no lyrics, 2 minutes` |
| `tense` | `tense` `uneasy` | `low drone and sparse percussive ticks, uneasy, almost no melody, instrumental, no vocals, no lyrics, 2 minutes` |
| `tense` *(2nd)* | `tense` `uneasy` | `the same unease with a bowed metal edge and a distant drum, instrumental, no vocals, no lyrics, 2 minutes` |
| `generic-soft` | `soft` `calm` | `soft pads, almost no motion, room for a voice, instrumental, no vocals, no lyrics, 2 minutes` |
| `soft-relax` | `soft` `calm` | `the same stillness with a single distant erhu line, instrumental, no vocals, no lyrics, 2 minutes` |

`court-mystery`'s five extra moods — `court`, `inquiry`, `illness`, `wicked`,
`wry` — are answered by the fifteen tracks that go in this pack (see its own
section); a mood a pack declares has to be answerable here, not in the sfx pack.

## Making a file

Generated on this machine, one prompt at a time from the tables above:
**Small-SFX** for a place (`-26`, under the voice) and for a moment (`-20`, in
front of it), **Small-Music** for a track (`-23` at 96k, and registered in the
pool so its tags reach the palette):

```sh
tools/gen-sound.sh setup                     # once
tools/gen-sound.sh one --prompt "<the prompt from a place or moment row>" \
  --as storm-2 --into assets/_extends/common/effects --level place
tools/gen-sound.sh one --prompt "<the prompt from a music row>" \
  --as market-bg-2 --into assets/music --level track --tags market,busy
```

A track's `--tags` are its row's tags, verbatim — they are what the palette
matches — and `--sound` defaults to the take's own name, so `market-bg-2` is a
second take of `market`. `--pool` defaults to `music-pool.json` beside the
destination directory.

For a moment, the length in the row is the length of the **finished** clip: the
model is asked for at least three seconds and the take is cut back to the row's
length, because a sub-second request comes back as a block of noise rather than
an event. A moment under two seconds is placed by its peak rather than by a
loudness measurement, which is what the pool's own short takes do.

A moment is also where this route is weakest, and it is worth knowing before
planning a batch: a one-shot a second or two long comes back with the wrong
*shape* more often than not — a knock where a break should roll, a swell where a
swallow should click. Averages hide that, which is how three `common` takes
shipped that read as a drone, a warble and a voice. `rock-break`,
`cloth-flutter` and `gulp` carry their recorded take alone for that reason, and
their second takes belong to a microphone. A **long** take is this route's
ground: a place, or a music pack's track.

That length also decides whether a prompt comes back as an **event** — a hit with
real silence behind it — or as a **block**, a wall of sound that never falls
away: one prompt is an event at 3.0 s and a block at 3.2 s, on the same seed. So
a take that arrives wrong is a length to re-roll before it is a prompt to
rewrite — and a generation is only about a second.

Pin a take worth keeping, or install one you already have:

```sh
# the same command then makes the same clip again
tools/gen-sound.sh one --prompt "<a row's prompt>" --as storm-2 \
  --into assets/_extends/common/effects --level place --seed 123456789
# or install a raw you already have, through the same cut, fade and level rule
tools/gen-sound.sh one --from refs/temp/cand2.wav --as wind-2 \
  --into assets/_extends/common/effects --level place
```

`--cfg` and `--negative` steer a generation and take the optimized runtime's
flags.

The Small-SFX and Small-Music weights are **gated** on Hugging Face: one click,
then one login.

```
click agree at https://huggingface.co/stabilityai/stable-audio-3-small-sfx
click agree at https://huggingface.co/stabilityai/stable-audio-3-small-music
export HF_TOKEN=hf_...        # or, in the engine: uv run hf auth login
```

No account is needed for the *same two tiers* through the `optimized/` runtimes
in the same checkout — they pull from the public
`stabilityai/stable-audio-3-optimized` — and `BM_SA_CLI` with
`BM_SA_CLI_FLAVOR=optimized` puts them behind the same command. §9 of
[SOUND.md](SOUND.md).

From a service instead, or from a recording:

```sh
# a place, -26 because it sits under the voice
I_TARGET=-26 TP_TARGET=-1 tools/normalize-audio.sh <src-dir> <dest-dir>
# a moment, -20 because it is in front
I_TARGET=-20 TP_TARGET=-1 tools/normalize-audio.sh <src-dir> <dest-dir>
```

Save as `<sound>-1.mp3`, `<sound>-2.mp3` — the number is a file index inside the
sound and nothing reads it. Generated files arrive at 44.1 kHz and the pools
expect `48000/1`.

```sh
cd rust && cargo test -p bm-core --test pack_gates -- --nocapture   # the gate
python3 tools/inspect-pool.py assets/_extends/craft                # files still missing
```
