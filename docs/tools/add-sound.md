# add-sound.py — register a clip in a pack

```bash
tools/add-sound.py [--apply] [--tags a,b] [--mode hit|overlap|trail] [--hold S] [--gain F]
                   [--once] [--alias a,b] [--alias-to tag]
                   <pack> place|moment <key> <clip> [clip...]
```

Example:

```bash
tools/add-sound.py craft moment paper-powder take1.wav take2.wav \
    --tags paper,packet,powder --alias folding-paper
```

Dry run by default; `--apply` writes. It spec-checks each clip (normalizing
through `normalize-audio.sh` when needed), files it as
`effects/<key>-<N>.mp3` / `injects/<key>-<N>.mp3`, registers the pool entry in
the file's own formatting, maps `--alias` words in `tag-aliases.json`, and
proves the row with `inspect-pool.py`. Afterwards `asset resolve` to fold the
pack into the live tree.
