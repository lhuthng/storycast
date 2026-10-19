# preset-publish.sh — publish a release to R2

```bash
R2_ACCOUNT_ID=… R2_ACCESS_KEY_ID=… R2_SECRET_ACCESS_KEY=… \
  tools/preset-publish.sh <kind> <name> <version|auto> <file...>
```

Example:

```bash
tools/preset-publish.sh sounds xianxia auto profiles/pack/xianxia.tar.zst
```

Layout per release: `preset/<kind>/<name>/<version>/<file>` plus the mutable
`preset/<kind>/<name>/latest.json` (`<kind>` is `sounds`, `models`,
`profiles` or `voices`). Versions are date tags, never overwritten — fix a bad
upload with a new version, roll back by re-pointing `latest.json`.

The script uploads each file, derives the tree manifest hash from the bundle,
writes + uploads `latest.json`, then verifies with a ranged GET using the
`storycast-preset-fetch/1.0` agent string. Optional env: `R2_BUCKET`
(default `storycast-presets`), `R2_PUBLIC_BASE`. Never commit the credentials.
