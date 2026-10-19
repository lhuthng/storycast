# normalize-audio.sh — bring clips to the house spec

```bash
I_TARGET=-26 TP_TARGET=-1 tools/normalize-audio.sh <src-dir> <dest-dir>   # places (beds, under the voice)
I_TARGET=-20 TP_TARGET=-1 tools/normalize-audio.sh <src-dir> <dest-dir>   # moments (hits, in front)
```

What it does to every clip: trim leading/trailing silence, short fades at both
edges (click guard), two-pass loudnorm to the target (linear gain — dynamics
survive), mono 48 kHz mp3, all metadata dropped. Clips under `PEAK_UNDER`
(default 0.5 s) are peak-placed instead of loudness-measured.

Files already in `<dest-dir>` are left alone — it is how a clip gets in, not a
re-encode pass. Needs `ffmpeg` on PATH.
