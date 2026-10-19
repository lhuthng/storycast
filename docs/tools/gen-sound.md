# gen-sound.sh — generate a clip with Stable Audio 3

```bash
tools/gen-sound.sh setup                          # once: engine venv + revision pin
tools/gen-sound.sh one --prompt "…" --as <stem> --into <pool-dir> --level place|moment [--seconds N] [--seed N]
tools/gen-sound.sh one --prompt "…" --as <stem> --into <music-dir> --level track --sound <key> --tags a,b
tools/gen-sound.sh one --from <wav> --as <stem> --into <pool-dir> --level place|moment   # install a raw you picked
tools/gen-sound.sh batch --list <tsv>             # one clip a line, resumable
tools/gen-sound.sh list                           # what is installed, and where
```

- Tier is decided by `--level`: Small-SFX for places/moments, Small-Music for
  tracks. Gated weights — agree once at `stabilityai/stable-audio-3-small-sfx`
  and `-small-music`, then `HF_TOKEN` (or `uv run hf auth login`).
- Moments are generated long (≥3 s) and cut back: sub-second requests come back
  as noise, not events. A wrong-shaped take is a length to re-roll first.
- Every clip is normalized on the way in (places −26 LUFS, moments −20);
  existing destination files are never touched. Register it next:
  `add-sound.py` for places/moments, `tools/add-music.sh` for tracks.
