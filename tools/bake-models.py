#!/usr/bin/env python3
"""Bake a flat `models/` directory out of the Hugging Face cache.

The weights arrive from HF in a content-addressed layout — `blobs/<sha256>` with
a `snapshots/<rev>/` tree of symlinks, plus a `.no_exist/` record of what was
asked for and absent. That is an implementation detail of `huggingface_hub`, and
shipping it means every worker has to reason about symlinks and revisions to find
a file it already knows the name of.

This flattens it: named files, one directory, plus a manifest of sizes and
hashes. Two consequences worth stating:

* **Provisioning can rsync it.** A worker needs no internet, no `hf_xel`, no
  `huggingface_hub` — the host resolves the model once and pushes the bytes.
* **The `.data` files must keep their names and stay beside their `.onnx`.** An
  ONNX graph with external weights refers to them by a relative path baked into
  the graph, so moving one without the other produces a load error that names
  neither.

Everything lands in **one** directory, codec included: the codec's filenames do
not collide with the backbone's, and a single path is one fewer thing for
provisioning and the server's CLI to get wrong.

    python3 tools/bake-models.py [--out models] [--check]

`--check` re-hashes an existing bake and reports drift without writing anything.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import pathlib
import shutil
import sys

HF = pathlib.Path.home() / ".cache/huggingface/hub"
BACKBONE = "models--pnnbao-ump--VieNeu-TTS-v3-Turbo"
CODEC = "models--OpenMOSS-Team--MOSS-Audio-Tokenizer-Nano-ONNX"

# The backbone's own files live in the `onnx_update` subfolder; the speaker
# encoder and denoiser sit at the repository root.
BACKBONE_SUB = [
    "onnx_update/vieneu_prefill.onnx",
    "onnx_update/vieneu_decode_step.onnx",
    "onnx_update/vieneu_acoustic_cached.onnx",
    "onnx_update/vieneu_backbone_shared.data",
    "onnx_update/vieneu_v3_heads.npz",
    "onnx_update/config.json",
    "onnx_update/tokenizer.json",
    "speaker_encoder.onnx",
    "denoiser.onnx",
]
CODEC_FILES = [
    "moss_audio_tokenizer_decode_full.onnx",
    "moss_audio_tokenizer_decode_shared.data",
    "moss_audio_tokenizer_decode_step.onnx",
    "moss_audio_tokenizer_encode.onnx",
    "moss_audio_tokenizer_encode.data",
    "codec_browser_onnx_meta.json",
]

# Not from HF: the G2P dictionary and the voice roster.
DICT = pathlib.Path(
    "rust/vendor/sea-g2p/python/sea_g2p/sea_g2p.bin"
)
DICT_FALLBACKS = [
    pathlib.Path(".venv/lib/python3.12/site-packages/sea_g2p/sea_g2p.bin"),
]
STORE = pathlib.Path(
    ".venv/lib/python3.12/site-packages/vieneu/assets/voices_v3_turbo.json"
)

# Copied into the bake but deliberately **not** recorded in the manifest.
#
# `models/voices.json` is the voice roster, and unlike everything else here it is
# not a bake output. Two reasons it cannot be one:
#
#   * `pool::bake_missing_voices` rewrites it on the inductor during provisioning
#     whenever the clone manifest names a voice the store lacks, so its bytes are
#     expected to change without a re-bake; and
#   * its source is a *pip-installed package* (`vieneu`), not a pinned revision —
#     which is why it is the one entry out of seventeen that had drifted here.
#
# A recorded `bytes` + `sha256` therefore describe a file the manifest cannot
# stand behind. Recording it cost two things: `--check` reported a spurious
# CHANGED after any enrollment, and every consumer had to special-case one entry
# (the artifact publish gate, and the `sha256sum -c` list provisioning writes).
# A receipt that is occasionally wrong is worse than a receipt that omits a
# field, so `voices.json` is copied for the sidecar to load and left out of the
# record entirely.
#
# What it is no longer covered by: `--check`. Its presence is checked instead by
# the sidecar at startup, the remote roster check in `provision`, and
# `bake_missing_voices`, which re-enrolls a declared voice the store has lost.
DERIVED = {"voices.json"}

ROOT = pathlib.Path(__file__).resolve().parent.parent


def snapshot(repo: str) -> pathlib.Path:
    d = HF / repo / "snapshots"
    if not d.is_dir():
        sys.exit(f"no snapshots for {repo}; download the model first")
    revs = sorted(p for p in d.iterdir() if p.is_dir())
    if not revs:
        sys.exit(f"{d} is empty")
    if len(revs) > 1:
        print(f"  note: {len(revs)} revisions of {repo}, using {revs[-1].name[:8]}")
    return revs[-1]


def sha256(p: pathlib.Path) -> str:
    h = hashlib.sha256()
    with open(p, "rb") as fh:
        for block in iter(lambda: fh.read(1 << 20), b""):
            h.update(block)
    return h.hexdigest()


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", default="models")
    ap.add_argument("--check", action="store_true")
    args = ap.parse_args()
    out = (ROOT / args.out).resolve()

    manifest_path = out / "manifest.json"
    if args.check:
        if not manifest_path.is_file():
            sys.exit(f"no manifest at {manifest_path}; nothing to check")
        man = json.loads(manifest_path.read_text())
        bad = 0
        for name, want in man["files"].items():
            p = out / name
            if not p.is_file():
                print(f"  MISSING {name}")
                bad += 1
                continue
            got = sha256(p)
            if got != want["sha256"]:
                print(f"  CHANGED {name}\n    want {want['sha256']}\n    got  {got}")
                bad += 1
        print(f"{len(man['files']) - bad}/{len(man['files'])} files match the manifest")
        return 1 if bad else 0

    if out.exists() and any(out.iterdir()):
        print(f"{out} is not empty; files will be overwritten")

    bb = snapshot(BACKBONE)
    cd = snapshot(CODEC)
    out.mkdir(parents=True, exist_ok=True)

    plan: list[tuple[pathlib.Path, str]] = []
    for rel in BACKBONE_SUB:
        plan.append((bb / rel, pathlib.Path(rel).name))
    for rel in CODEC_FILES:
        plan.append((cd / rel, rel))

    dict_src = ROOT / DICT
    if not dict_src.is_file():
        dict_src = next((ROOT / p for p in DICT_FALLBACKS if (ROOT / p).is_file()), None)
    if dict_src is None:
        sys.exit(
            "no sea_g2p.bin found. Install sea-g2p==0.9.1, or fetch it from the "
            "pinned upstream commit (hash in rust/vendor/sea-g2p/VENDORED.md)."
        )
    plan.append((dict_src, "sea_g2p.bin"))
    plan.append((ROOT / STORE, "voices.json"))

    files: dict[str, dict] = {}
    total = 0
    for src, name in plan:
        if not src.is_file():
            # Optional: the encoder and denoiser are only needed for cloning, and
            # a missing one should not stop a preset-only bake.
            optional = name in ("speaker_encoder.onnx", "denoiser.onnx", "codec_browser_onnx_meta.json")
            print(f"  {'skip' if optional else 'MISSING'} {name} ({src})")
            if not optional:
                return 1
            continue
        dst = out / name
        shutil.copy2(src, dst)
        size = dst.stat().st_size
        if name in DERIVED:
            # Copied, so the sidecar can load it — never recorded. See DERIVED.
            print(f"  {size / 1048576:8.1f} MB  {name}  (derived, not recorded)")
            continue
        total += size
        files[name] = {"bytes": size, "sha256": sha256(dst)}
        print(f"  {size / 1048576:8.1f} MB  {name}")

    manifest = {
        "_note": (
            "Baked by tools/bake-models.py from the Hugging Face cache. The .data "
            "files must keep these names and stay beside their .onnx: the graph "
            "refers to them by a relative path baked into the file. Verify with "
            "`python3 tools/bake-models.py --check`. `voices.json` sits in this "
            "directory but is deliberately absent from `files`: it is derived "
            "state, not a bake output (see DERIVED in the script)."
        ),
        "backbone_rev": bb.name,
        "codec_rev": cd.name,
        "total_bytes": total,
        "files": files,
    }
    manifest_path.write_text(json.dumps(manifest, indent=1, ensure_ascii=False) + "\n")
    print(f"\n{len(files)} files, {total / 1048576:.0f} MB -> {out}")
    print(f"manifest: {manifest_path}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
