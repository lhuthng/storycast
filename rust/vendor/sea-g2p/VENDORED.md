# Vendored: sea-g2p

Vietnamese text normalization and grapheme-to-phoneme conversion, for the
Python-free TTS server. This directory is upstream's source with **one** patch
applied — nothing else — so a future upstream bump is a re-copy plus a re-run
of the patch script.

| | |
|---|---|
| Upstream | https://github.com/pnnbao97/sea-g2p |
| Commit | `ee2e80b0ac47f1d9403f5c5fd88ecd6265e4e6b1` |
| Crate | `sea-g2p-rs` 0.9.1 (lib `sea_g2p_rs`) — **not published to crates.io** |
| Licence | Apache-2.0 (`LICENSE`, kept verbatim) |
| Patch | `tools/vendor-sea-g2p-patch.py`, 15 anchor-checked sites |
| Consumer | `rust/crates/bm-tts` (`default-features = false`) |

## Why it is vendored

Not a crates.io dependency, so there is no `version = "0.9.1"` to point at. The
alternatives were worse: a git dependency cannot carry a patch without a fork to
fork from, and a build-time download-and-patch is not reproducible.

The Python wheel (`pip install sea-g2p`) ships `sea_g2p_rs.abi3.so`, a PyO3
extension module. It is the same Rust code, but calling it from Rust means
embedding CPython — which is the thing this whole exercise removes.

## What the patch does

PyO3 goes from a hard dependency to an opt-in feature:

```toml
[features]
default = []
python = ["dep:pyo3"]
```

The binding surface is tiny and entirely at the edges — two `use` lines, six
attribute sites, and three methods that take a `Python` handle. Nothing in
`src/core/`, `src/g2p/` or `src/lang/` imports PyO3 at all. In `src/lib.rs` the
gated items get `#[cfg(feature = "python")]`; in `src/lang/vi/mod.rs` the
`Normalizer` keeps its plain-Rust body and its attributes become
`#[cfg_attr(feature = "python", pyclass)]`, so the type is an ordinary struct
when the feature is off.

Two consequences worth knowing:

* **`default = []`, not `["python"]`.** The default *was* PyO3, and leaving it
  would make `cargo build --workspace` link libpython through a path
  dependency. Building the wheel again needs `--features python` — add
  `features = ["python"]` under `[tool.maturin]` in upstream's `pyproject.toml`
  if that ever matters.
* `[build-dependencies] pyo3-build-config` was removed. There is no `build.rs`
  in the tree, so it was already inert.

The crate also gained an empty `[workspace]` table and is listed under
`exclude` in `rust/Cargo.toml`: cargo refuses to see a workspace root nested
inside another one otherwise. It is excluded rather than a member because
`cargo build --workspace` must not build it with its own default features, and
its upstream test suite needs the 60 MB dictionary that is not in this repo.

## The dictionary

`sea_g2p.bin` — 62,829,820 bytes, the memory-mapped phoneme dictionary the G2P
reads at startup. **Not committed**, and not needed to build; it is a runtime
path argument.

```
sha256  4346e690d0711ebc5231e7a42c5c88aaf6e40377e894b4617c018fd81c6f4096
```

Verified byte-identical across the upstream repository at the pinned commit and
the `sea-g2p==0.9.1` wheel, so either is a sound source. Resolution order for
tooling that needs it (`tools/g2p-parity.py`): `models/sea_g2p.bin`, then
`rust/vendor/sea-g2p/python/sea_g2p/sea_g2p.bin` (upstream's own layout, which
is what upstream's `tests/*.rs` hard-code), then the installed wheel.

Upstream can regenerate it from `scripts/` plus the `thai/`, `indo/` and
frequency data in the repo; those build inputs are deliberately not vendored,
since we ship the artifact and can verify it by hash.

## Re-applying on an upstream bump

```sh
git clone --depth 1 https://github.com/pnnbao97/sea-g2p /tmp/sea-g2p-src
rm -rf rust/vendor/sea-g2p
mkdir -p rust/vendor/sea-g2p
cp -R /tmp/sea-g2p-src/{Cargo.toml,LICENSE,README.md,src,tests} rust/vendor/sea-g2p/
python3 tools/vendor-sea-g2p-patch.py rust/vendor/sea-g2p
```

The patch script asserts every anchor exists exactly once, so a moved line
fails loudly instead of silently producing a build that links libpython. If an
anchor has moved, fix the script rather than the vendored file — the vendored
file is meant to be reproducible from upstream plus the script.

Then re-run `tools/g2p-parity.py` and update the commit and hash above.

## Verifying this copy is unmodified

The claim at the top of this file — upstream plus one patch, nothing else — is
checkable, and worth re-checking after any edit here:

```sh
git clone https://github.com/pnnbao97/sea-g2p /tmp/sea-g2p-src
git -C /tmp/sea-g2p-src checkout ee2e80b0ac47f1d9403f5c5fd88ecd6265e4e6b1

rm -rf /tmp/reproduce && mkdir /tmp/reproduce
cd /tmp/sea-g2p-src
cp -R Cargo.toml LICENSE README.md src tests /tmp/reproduce/

cd -   # back to the repo root
python3 tools/vendor-sea-g2p-patch.py /tmp/reproduce

diff -r /tmp/reproduce rust/vendor/sea-g2p   # only VENDORED.md, which is ours
```

Last run 2026-09-18: **three files differ, 30 lines, and not one of them is
code** — every change is a `#[cfg]`/`#[cfg_attr]` attribute or manifest
metadata:

| file | lines | what |
|---|---|---|
| `Cargo.toml` | 10 | `[workspace]`; `[features] default = []` / `python`; `optional = true`; drop the inert `pyo3-build-config` build-dep |
| `src/lib.rs` | 6 | six `#[cfg(feature = "python")]` gates on the binding items |
| `src/lang/vi/mod.rs` | 14 | the same, as `#[cfg_attr]`, so `Normalizer` stays an ordinary struct |

No function body, expression or type is touched. If a future diff here shows a
changed statement rather than a changed attribute, that is the signal that this
copy has drifted and the fix belongs in the patch script.

## What was verified, and how

`tools/g2p-parity.py` — 259 lines of real chapter prose from `data/chapters/`
plus twenty normalizer edge cases (numbers, dates, units, money, ranges, URLs,
emails, English code-switching, punctuation shapes), run through both this crate
and the `sea-g2p==0.9.1` wheel: **259 identical, 0 differing**.

One trap that check found: the normalizer must be constructed with **no**
dictionary path. `vieneu_utils.PuncNormalizer` and `sea_g2p.SEAPipeline` both
call `Normalizer(lang=lang)`, so `init_norm_dict` is never invoked and the
normalizer runs on its built-in whitelist. Passing the dictionary — which looks
like the more careful thing to do — changes the output for paths, URLs and
emails inside Vietnamese sentences. Matching the reference means passing `None`.
