#!/usr/bin/env python3
"""Render the same text and voice in both implementations, and diff the audio.

The last gate, and the only one that exercises everything at once: chunking →
phonemes → codes → codec → joined waveform, with a real preset voice from the
shipped roster.

Temperature 0, so both sides are deterministic and the comparison means
something. A preset voice is used rather than a clone because its speaker
embedding and reference codes are precomputed in the store — no fbank, no
speaker encoder, no codec encode, so a mismatch here is the pipeline and not the
enrollment path.

**Expect the same residual as the frame comparison.** The speaker anchor comes
out of a BLAS gemv and differs by ~1.5e-07 from a sequential loop; deep in a long
sequence that can flip an argmax where two logits are near-tied. Short renders
should be exact; a long one may diverge on one code of one frame.

    python3 tools/render-parity.py [--texts N] [--voice NAME]
"""
from __future__ import annotations

import argparse
import glob
import json
import pathlib
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
RUST_BIN = ROOT / "rust/target/debug/bm-tts-render"
WORK = pathlib.Path("/tmp/bm-render-parity")

HF = pathlib.Path.home() / ".cache/huggingface/hub"
BACKBONE_GLOB = str(HF / "models--pnnbao-ump--VieNeu-TTS-v3-Turbo/snapshots/*/onnx_update")
CODEC_GLOB = str(HF / "models--OpenMOSS-Team--MOSS-Audio-Tokenizer-Nano-ONNX/snapshots/*")
STORE = ROOT / ".venv/lib/python3.12/site-packages/vieneu/assets/voices_v3_turbo.json"
DICT_CANDIDATES = [
    ROOT / "models/sea_g2p.bin",
    ROOT / "rust/vendor/sea-g2p/python/sea_g2p/sea_g2p.bin",
    ROOT / ".venv/lib/python3.12/site-packages/sea_g2p/sea_g2p.bin",
]

SR = 48_000

TEXTS = [
    "Không sao.",
    "Hắn cười. Cô ấy khóc!",
    "Chương một.\nNội dung dài hơn hẳn để đoạn sau không bị coi là ngắn.",
    'Có phải kiểu như: "Rồi sao nữa? Mình phải làm đến bao giờ?", đúng không anh?',
    "Hắn [cười] rồi nói tiếp.",
    "Giá là 4.200,5 điểm. Hẹn lúc 8.30 sáng nhé.",
]


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--texts", type=int, default=len(TEXTS))
    ap.add_argument("--voice", default=None)
    args = ap.parse_args()

    if not RUST_BIN.is_file():
        sys.exit(f"build it first: cargo build -p bm-tts --bin bm-tts-render ({RUST_BIN})")
    if not STORE.is_file():
        sys.exit(f"no voice store at {STORE}")
    d = next((p for p in DICT_CANDIDATES if p.is_file()), None)
    if d is None:
        sys.exit("no sea_g2p.bin found")

    backbone = sorted(glob.glob(BACKBONE_GLOB))[-1]
    codec = sorted(glob.glob(CODEC_GLOB))[-1]
    texts = TEXTS[: args.texts]

    store = json.loads(STORE.read_text())
    voice = args.voice or store.get("default_voice") or sorted(store["presets"])[0]
    print(f"voice {voice!r}, {len(texts)} texts, temperature 0")

    import numpy as np
    from vieneu import Vieneu

    tts = Vieneu()
    WORK.mkdir(parents=True, exist_ok=True)
    want = []
    for i, t in enumerate(texts):
        wav = tts.infer(
            t,
            voice=voice,
            temperature=0.0,
            top_k=25,
            top_p=0.95,
            max_new_frames=300,
            repetition_penalty=1.2,
            repetition_window=64,
            max_chars=256,
            silence_p=0.15,
            crossfade_p=0.0,
            apply_watermark=True,
            # Force the sequential path; batching is a GPU concern.
            batch_size=1,
        )
        a = np.asarray(wav, dtype="<f4")
        (WORK / f"py.{i}.f32").write_bytes(a.tobytes())
        want.append(a)
        print(f"  python {i}: {a.size} samples ({a.size / SR:.2f}s), peak {np.abs(a).max():.4f}")

    print("\nrust…")
    # Through a file, not stdin: a paragraph contains newlines and a
    # line-per-text protocol would split it into two renders.
    (WORK / "texts.json").write_text(json.dumps(texts))
    got = subprocess.run(
        [
            str(RUST_BIN),
            backbone,
            "--codec",
            codec,
            "--dict",
            str(d),
            "--voices",
            str(STORE),
            "--voice",
            voice,
            "--temp",
            "0",
            "--texts",
            str(WORK / "texts.json"),
            "--wav",
            str(WORK / "rs"),
            "--raw",
            str(WORK / "rs"),
        ],
        capture_output=True,
        text=True,
        timeout=3600,
    )
    if got.returncode != 0:
        sys.exit(f"bm-tts-render failed:\n{got.stderr[-2000:]}")
    for line in got.stderr.splitlines():
        if "line " in line or "voice:" in line:
            print("  " + line.strip())

    def compare(label: str, extra: list[str]) -> int:
        if extra:
            print(f"\nre-running with the reference's own anchor forced…")
            r = subprocess.run(
                [
                    str(RUST_BIN), backbone, "--codec", codec, "--dict", str(d),
                    "--voices", str(STORE), "--voice", voice, "--temp", "0",
                    "--texts", str(WORK / "texts.json"),
                    "--raw", str(WORK / "rs2"), *extra,
                ],
                capture_output=True, text=True, timeout=3600,
            )
            if r.returncode != 0:
                sys.exit(f"bm-tts-render (forced anchor) failed:\n{r.stderr[-1500:]}")
            return count(label, WORK / "rs2")
        return count(label, WORK / "rs")

    def count(label: str, prefix: pathlib.Path) -> int:
        n_bad = 0
        for i, w in enumerate(want):
            f = prefix.with_name(f"{prefix.name}.{i}.f32")
            if not f.is_file():
                sys.exit(f"{label}: no audio for line {i}")
            b = np.fromfile(f, dtype="<f4")
            if b.size != w.size:
                print(f"  {label} line {i}: samples python {w.size} vs rust {b.size}")
                n_bad += 1
                continue
            worst = float(np.abs(w.astype(np.float64) - b.astype(np.float64)).max())
            print(f"  {label} line {i}: {w.size} samples, maxabs {worst:.3e}, exact {np.array_equal(w, b)}")
            if worst > 1e-4:
                n_bad += 1
        print(f"  {label}: {len(want) - n_bad}/{len(want)} lines within tolerance")
        return n_bad

    bad = count("as-shipped", WORK / "rs")

    if bad:
        # Hand both sides the reference's own anchor: if the renders then match,
        # the pipeline is exact and the residual is one matmul's summation order.
        anchor = python_anchor(store, voice)
        (WORK / "anchor.f32").write_bytes(anchor)
        bad = compare("anchor forced", ["--anchor", str(WORK / "anchor.f32")])

    print("\nRESULT:", "IDENTICAL" if bad == 0 else "DIVERGENT")
    return 1 if bad else 0


def python_anchor(store: dict, voice: str) -> bytes:
    """The reference's own speaker anchor for a preset, as raw f32."""
    import glob as _glob

    import numpy as np
    from vieneu._v3_turbo_engine.onnx_runtime_lite import OnnxV3LiteEngine

    engine = OnnxV3LiteEngine(
        onnx_dir=sorted(_glob.glob(BACKBONE_GLOB))[-1],
        codec_dir=sorted(_glob.glob(CODEC_GLOB))[-1],
    )
    emb = np.asarray(store["presets"][voice]["speaker_emb"], dtype="<f4")
    return engine._speaker_anchor(emb).astype("<f4").tobytes()


if __name__ == "__main__":
    sys.exit(main())
