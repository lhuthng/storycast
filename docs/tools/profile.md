# profile.sh — cut, fetch and verify releases

```bash
tools/profile.sh pack <name> --piece pack|adapter [--level N] [--version V] [--dep] [--force]
tools/profile.sh fetch <name> [@version] [--piece p] [--repo owner/name]
tools/profile.sh unpack <name> [--piece p]     # verify, then replace the live piece
tools/profile.sh verify <name> [--piece p]     # manifest vs contents + AppleDouble gate
tools/profile.sh list
tools/profile.sh update [--dry-run] [--force]  # pull newest releases of the live closure's deps
```

- A release is transfer + archive only; day to day the pipeline reads the
  unpacked live tree, and runners gate on the `.bm/profile` binding hash.
- `pack` refuses a tree whose deps moved unless `--force` (packing behind a
  moved parent, on purpose). `--dep` releases one `assets/_extends/<name>/`
  tree as a sanitized root pack.
- `fetch` reads `packs_release` (R2 URL or `owner/name`); the file only gets
  its real name after its sha256 matches. Needs `tar`, `zstd`, `python3`.
- After packing a sound pack: `preset-publish.sh sounds <name> <version>
  profiles/pack/<name>.tar.zst`.
