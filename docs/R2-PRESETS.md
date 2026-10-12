# Preset hosting on Cloudflare R2

Profiles, model bundles, voice packs and sound packs live in one R2 bucket,
not in git and not in GitHub releases. Releases cap files at 2GB and throttle
large downloads; the repo stays source-only. Provision boxes pull over plain
HTTPS with the sha256 checks they already do.

Decisions baked in: public bucket (packs hold no secrets, so boxes carry no
credentials), one bucket for all pack kinds, content hashes in the key layout
below.

## 1. Account and bucket

1. Sign up at cloudflare.com (free plan is enough).
2. Open R2 Object Storage, create a bucket named `storycast-presets`.
3. Stay on defaults otherwise. Versioning is not needed: versions are keys.

## 2. Public access and a stable URL

1. In the bucket settings, enable public access (R2 calls this a public
   bucket or a public development URL plus an allowed custom domain).
2. Do not ship on the `*.r2.dev` URL: it is rate-limited and not for
   production traffic.
3. Connect a custom domain you already have on Cloudflare, e.g.
   `presets.example.com`, and point it at the bucket. This domain is the
   only value the code needs. Until it exists, stop here: everything below
   hangs off it.

## 3. Key layout

```
preset/<kind>/<name>/<version>/<file>
preset/<kind>/<name>/latest.json
```

- `<kind>` is one of `models`, `voices`, `profiles`, `sounds`.
- `<version>` is a date tag like `2026-10-11`, never `latest` and never
  overwritten. A bad upload is fixed by publishing a new version, not by
  replacing files.
- `latest.json` is the only mutable key per pack. Shape:

```json
{
  "version": "2026-10-11",
  "files": {
    "models.tar.zst": {
      "key": "preset/models/vieneu/2026-10-11/models.tar.zst",
      "sha256": "<hex>",
      "bytes": 380110860
    }
  }
}
```

The fetcher resolves the pack through `latest.json`, then verifies every
file against the listed sha256 exactly as provision verifies releases
today. The hash in the file list is the integrity story, not HTTPS alone.

## 4. Uploads

For the first migration the dashboard upload is fine. For repeatable
publishing, configure rclone once:

```
rclone config  # new remote, type s3, provider Cloudflare,
               # endpoint https://<account-id>.r2.cloudflarestorage.com,
               # access key + secret from an R2 API token with write scope
```

Publish one version (example: the vieneu models bundle):

```
VER=2026-10-11
sha256sum models.tar.zst
rclone copyto models.tar.zst r2:storycast-presets/preset/models/vieneu/$VER/models.tar.zst
```

Then write `latest.json` locally with the real hash and byte count and
upload it to `preset/models/vieneu/latest.json`. Verify with curl before
telling anyone:

```
curl -s https://presets.example.com/preset/models/vieneu/latest.json
curl -s -o /dev/null -w "%{http_code} %{size_download}\n" \
  https://presets.example.com/preset/models/vieneu/$VER/models.tar.zst
```

## 5. Wiring the fetcher (done)

- `models_release` / `packs_release` take either `owner/name` (GitHub, as
  before) or an `https://` base URL (R2). A URL resolves through the pack's
  `latest.json` at provision time; a miss falls back to pushing over the
  uplink, exactly like an unreachable GitHub release.
- The box still fetches one URL with one hash (`fetch-artifact --expect`):
  the indirection stays inductor-side, and every fetch sends the
  `storycast-preset-fetch/1.0` agent string the bucket's bot protection
  expects. Python-based tooling must set it too: bare `Python-urllib`
  answers 403 on file paths.
- After publishing, purge `preset/*` on the domain before verifying: edge
  cache can freeze a mid-propagation read, and the verify step then fails
  against stale bytes while the bucket is already correct. Verify with a
  cache-buster if a purge is not at hand.

## 6. If a pack must go private later

Keep it out of the public bucket. Options in increasing order of machinery:
unlisted key prefix (obscurity, not security), a second private bucket with
short-lived presigned URLs minted by the inductor, or Cloudflare Access in
front of the domain. The fetcher gains an auth header, the layout stays.

## 7. Costs and limits to watch

Free tier covers this several times over: 10GB storage, about a million
reads a month, no egress fees. Watch: total stored past 10GB, and the
`*.r2.dev` URL if anyone wires tooling to it before the custom domain
lands. R2 has no file-size problem at these scales (the models bundle is
under 400MB), which is the whole reason releases stopped fitting.

## 8. Rollback

Every version key is immutable and `latest.json` is one small file, so a
rollback is re-uploading the previous `latest.json`. Nothing is ever
deleted in a hurry: keep at least two versions live per pack.
