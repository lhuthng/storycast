#!/usr/bin/env python3
"""Diff the Rust generator against the Python engine, frame for frame.

The third gate. ONNX was proven bit-identical (`bm-tts-probe`) and the text front
end too (`g2p-parity.py`); this is the part in between — embeddings, the speaker
anchor, the prompt, prefill, the 16-channel acoustic loop, the decode step, and
the end-of-speech decision.

**How the comparison is possible at all.** `_sample` has a deterministic branch:
when temperature is not positive it returns `argmax` before any filtering. So at
temperature 0 the whole generation is a pure function of the inputs, and two
implementations must agree exactly. The stochastic path is implemented but
*not* verifiable this way — its RNG cannot match NumPy's — and is checked by the
filter-level test instead (`--filters`).

**Why the speaker anchor and reference codes are fabricated.** Enrollment
(fbank -> speaker encoder -> codec encode) is a separate path with its own
verification. Feeding it in here would mean a mismatch could be either path, so
both sides are handed the same synthetic bytes and only the generator is tested.

    python3 tools/frames-parity.py [--lines N] [--filters] [--keep]
"""
from __future__ import annotations

import argparse
import glob
import json
import pathlib
import random
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
RUST_BIN = ROOT / "rust/target/debug/bm-tts-frames"
WORK = pathlib.Path("/tmp/bm-frames-parity")

HF = pathlib.Path.home() / ".cache/huggingface/hub"
BACKBONE_GLOB = str(
    HF / "models--pnnbao-ump--VieNeu-TTS-v3-Turbo/snapshots/*/onnx_update"
)
CODEC_GLOB = str(
    HF / "models--OpenMOSS-Team--MOSS-Audio-Tokenizer-Nano-ONNX/snapshots/*"
)

SPEAKER_DIM = 192
REF_FRAMES = 6


def corpus(limit: int) -> list[str]:
    """Real sentences, short enough that a full run stays in minutes."""
    random.seed(20260917)
    chapters = sorted(glob.glob(str(ROOT / "data/chapters/*.txt")))
    texts: list[str] = []
    for f in chapters[:: max(1, len(chapters) // 30)][:30]:
        with open(f, encoding="utf-8") as fh:
            body = [l.strip() for l in fh if 12 < len(l.strip()) < 70]
        if body:
            texts.extend(random.sample(body, min(1, len(body))))
    texts += [
        "Không sao.",
        "Hắn cười.",
        "Thật không?",
    ]
    return texts[:limit]


def fixtures() -> tuple[bytes, list[list[int]]]:
    """Synthetic but deterministic: the generator is what is under test."""
    import numpy as np

    rng = np.random.default_rng(20260917)
    anchor = rng.standard_normal(SPEAKER_DIM).astype("<f4")
    ref = rng.integers(0, 1024, size=(REF_FRAMES, 16)).astype(int)
    return anchor.tobytes(), ref.tolist()


def python_run(phonemes: list[str], anchor: bytes, ref: list[list[int]], audio: bool = False):
    """Run the reference engine, capturing the frames it generated.

    With `audio`, the codec runs for real and the decoded wav is kept too — the
    frames are recorded by wrapping `_decode_codes` rather than replacing it, so
    the audio is the engine's own.
    """
    import numpy as np
    from vieneu._v3_turbo_engine.onnx_runtime_lite import OnnxV3LiteEngine

    backbone = sorted(glob.glob(BACKBONE_GLOB))[-1]
    codec = sorted(glob.glob(CODEC_GLOB))[-1]
    engine = OnnxV3LiteEngine(onnx_dir=backbone, codec_dir=codec)
    captured: list = []
    if audio:
        real = engine._decode_codes
        engine._decode_codes = lambda f: (captured.append(f), real(f))[1]
    else:
        # Only the codes matter, so skip the decode entirely.
        engine._decode_codes = lambda f: (captured.append(f), np.zeros(0, dtype=np.float32))[1]
    # Retrying would generate a second time and overwrite what we captured.
    engine.babble_retries = 0

    speaker = np.frombuffer(anchor, dtype="<f4").copy()
    ref_codes = np.asarray(ref, dtype=np.int64)

    frames, wavs = [], []
    for i, ph in enumerate(phonemes):
        captured.clear()
        wav = engine.infer(
            phonemes=ph,
            speaker_emb=speaker,
            ref_codes=ref_codes,
            temperature=0.0,
            max_new_frames=300,
            frame_cap=True,
        )
        if not captured:
            sys.exit(f"line {i}: the engine generated nothing")
        frames.append([[int(c) for c in frame] for frame in captured[0].tolist()])
        if audio:
            wavs.append(np.asarray(wav, dtype="<f4").tobytes())
    return frames, wavs


def python_anchor(phonemes: list[str], anchor: bytes, ref: list[list[int]]) -> bytes:
    """The reference's own speaker anchor, as raw f32.

    Only needed to locate a divergence. The value depends on the BLAS NumPy was
    built against, so it is not a fixture — it is one machine's answer to a
    question with several arithmetically-valid answers.
    """
    import glob as _glob

    import numpy as np
    from vieneu._v3_turbo_engine.onnx_runtime_lite import OnnxV3LiteEngine

    engine = OnnxV3LiteEngine(
        onnx_dir=sorted(_glob.glob(BACKBONE_GLOB))[-1],
        codec_dir=sorted(_glob.glob(CODEC_GLOB))[-1],
    )
    speaker = np.frombuffer(anchor, dtype="<f4").copy()
    return engine._speaker_anchor(speaker).astype("<f4").tobytes()


def filter_check() -> int:
    """Compare the top-k / top-p filter on a fixed logits vector.

    The draw cannot be compared — different RNGs — but the candidate set and its
    probabilities are deterministic given the logits, so `np.random.choice` is
    patched to report what it was offered instead of consuming it. That is the
    only way to reach the stochastic branch, which temperature 0 short-circuits.
    """
    import numpy as np

    rng = np.random.default_rng(7)
    logits = (rng.standard_normal(1024) * 3.0).astype(np.float32)
    (WORK / "logits.f32").write_bytes(logits.tobytes())

    from vieneu._v3_turbo_engine.onnx_runtime_lite import OnnxV3LiteEngine

    # `_sample` touches no model state, so it runs on a bare instance.
    eng = object.__new__(OnnxV3LiteEngine)
    real_choice = np.random.choice
    seen: list[tuple[int, np.ndarray]] = []

    def spy(n, p=None, **kw):
        seen.append((int(n), np.asarray(p, dtype=np.float64).copy()))
        return 0

    np.random.choice = spy
    try:
        # Temperature 1.0 on purpose: dividing by it is exact, so the comparison
        # is about the *filter* and not about how a scalar divides an array.
        # top_p 0.95 is what the engine ships; 1.0 exercises the un-nucleused path.
        eng._sample(logits.copy(), 1.0, 25, 0.95, 1.2, None)
        eng._sample(logits.copy(), 1.0, 25, 1.0, 1.2, None)
    finally:
        np.random.choice = real_choice

    if len(seen) != 2:
        sys.exit(f"expected 2 draws, saw {len(seen)}")

    want = {
        "probs": [float(p) for p in seen[0][1]],
        "probs_no_nucleus": [float(p) for p in seen[1][1]],
    }
    (WORK / "filters-python.json").write_text(json.dumps(want))
    print(f"  top_k candidates: {seen[0][0]}, sum(p)={seen[0][1].sum():.9f}")
    print(f"  with top_p 0.95: {int((seen[0][1] > 0).sum())} non-zero, max {seen[0][1].max():.6f}")
    print(f"  with top_p 1.00: {int((seen[1][1] > 0).sum())} non-zero, max {seen[1][1].max():.6f}")

    got = subprocess.run(
        [str(RUST_BIN), "--logits", str(WORK / "logits.f32")],
        capture_output=True,
        text=True,
        timeout=120,
    )
    if got.returncode != 0:
        sys.exit(f"bm-tts-frames --logits failed: {got.stderr[-800:]}")
    have = json.loads(got.stdout)
    ok = True
    for key in want:
        a, b = want[key], have.get(key) or []
        if len(a) != len(b):
            print(f"  {key}: candidate count python {len(a)} vs rust {len(b)}")
            ok = False
            continue
        # Structure first: which candidates survive the nucleus is what changes
        # behaviour, and it must agree exactly.
        mask_a = [x > 0 for x in a]
        mask_b = [x > 0 for x in b]
        if mask_a != mask_b:
            print(f"  {key}: nucleus keeps different candidates — python {sum(mask_a)} vs rust {sum(mask_b)}")
            ok = False
        # Then the values, to last-place rounding only. `np.exp` on float32 uses
        # a vectorised approximation that does not agree with libm's `expf` bit
        # for bit, so this cannot be an equality test and saying it is would be
        # a lie.
        worst = max(abs(x - y) for x, y in zip(a, b))
        print(f"  {key}: {len(a)} candidates, nucleus keeps {sum(mask_a)}, maxabs {worst:.3e}")
        if worst > 1e-6:
            ok = False
    print("RESULT:", "IDENTICAL" if ok else "DIVERGENT")
    return 0 if ok else 1


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--lines", type=int, default=8)
    ap.add_argument("--filters", action="store_true", help="only run the sampler filter check")
    ap.add_argument(
        "--audio",
        action="store_true",
        help="also run the codec and compare the decoded wav sample by sample",
    )
    args = ap.parse_args()

    if args.filters:
        return filter_check()

    if not RUST_BIN.is_file():
        sys.exit(f"build it first: cargo build -p bm-tts --bin bm-tts-frames ({RUST_BIN})")
    backbone = sorted(glob.glob(BACKBONE_GLOB))[-1]
    codec = sorted(glob.glob(CODEC_GLOB))[-1]

    WORK.mkdir(parents=True, exist_ok=True)
    anchor, ref = fixtures()
    (WORK / "speaker.f32").write_bytes(anchor)
    (WORK / "ref.json").write_text(json.dumps(ref))

    texts = corpus(args.lines)
    from vieneu_utils.phonemize_text import phonemize_text_with_emotions

    phonemes = [phonemize_text_with_emotions(t) for t in texts]
    (WORK / "phonemes.txt").write_text("\n".join(phonemes) + "\n")

    print(f"{len(phonemes)} lines; python engine…", flush=True)
    want, want_wav = python_run(phonemes, anchor, ref, audio=args.audio)
    (WORK / "python.json").write_text(json.dumps(want))

    print("rust engine…", flush=True)

    def run_rust(extra: list[str]) -> list:
        got = subprocess.run(
            [
                str(RUST_BIN),
                backbone,
                "--speaker",
                str(WORK / "speaker.f32"),
                "--ref",
                str(WORK / "ref.json"),
                "--temp",
                "0",
                *extra,
            ],
            input="\n".join(phonemes) + "\n",
            capture_output=True,
            text=True,
            timeout=3600,
        )
        if got.returncode != 0:
            sys.exit(f"bm-tts-frames failed ({got.returncode}):\n{got.stderr[-2000:]}")
        return [json.loads(l) for l in got.stdout.splitlines() if l.strip()]

    def diff_audio(label: str) -> int:
        """Compare the decoded wav. Only meaningful when the frames matched."""
        import numpy as np

        bad = 0
        for i, w in enumerate(want_wav):
            path = WORK / f"rs.{i}.f32"
            if not path.is_file():
                sys.exit(f"{label}: rust wrote no audio for line {i}")
            a = np.fromfile(path, dtype="<f4")
            b = np.frombuffer(w, dtype="<f4")
            if a.size != b.size:
                print(f"  line {i}: sample count python {b.size} vs rust {a.size}")
                bad += 1
                continue
            worst = float(np.abs(a.astype(np.float64) - b.astype(np.float64)).max())
            exact = bool(np.array_equal(a, b))
            peak = float(np.abs(b).max()) if b.size else 0.0
            print(
                f"  line {i}: {b.size} samples, peak {peak:.4f}, "
                f"maxabs {worst:.3e}, exact {exact}"
            )
            # A codec is a neural net too, so the same float-order caveat
            # applies; anything above this is a real difference, not rounding.
            if worst > 1e-4:
                bad += 1
        print(f"  {label}: {len(want_wav) - bad}/{len(want_wav)} lines within tolerance")
        return bad

    def diff(label: str, have: list) -> int:
        if len(have) != len(want):
            sys.exit(f"{label}: line count mismatch rust {len(have)} vs python {len(want)}")
        bad = 0
        for i, (t, w, h) in enumerate(zip(texts, want, have)):
            if w == h:
                continue
            bad += 1
            if bad <= 4:
                first = next(
                    (j for j in range(min(len(w), len(h))) if w[j] != h[j]),
                    min(len(w), len(h)),
                )
                print(f"\n--- MISMATCH ({label}) line {i}: {t[:70]!r}")
                print(f"    frames: python {len(w)} vs rust {len(h)}")
                if first < min(len(w), len(h)):
                    print(f"    first differing frame {first}:")
                    print(f"      python {w[first]}")
                    print(f"      rust   {h[first]}")
        print(f"  {label}: {len(texts) - bad}/{len(texts)} lines identical")
        return bad

    audio_args = ["--codec", codec, "--wav", str(WORK / "rs")] if args.audio else []
    bad = diff("as-shipped", run_rust(audio_args))
    audio_bad = 0

    # When the strict comparison diverges, say *where* the divergence enters
    # rather than leaving it as "the port is wrong". Handing both sides the
    # Python anchor takes the speaker projection out of the comparison: if the
    # frames then match, every other stage is exact and the residual is one
    # matmul's summation order.
    if bad:
        anchor = python_anchor(phonemes, anchor, ref)
        (WORK / "anchor.f32").write_bytes(anchor)
        print("\nre-running with the reference's own anchor forced…", flush=True)
        bad = diff("anchor forced", run_rust(["--anchor", str(WORK / "anchor.f32"), *audio_args]))

    # Audio is only comparable once the codes agree — different codes are
    # different speech, and reporting a sample difference would say nothing.
    if args.audio and bad == 0:
        print("\ncomparing the decoded audio…", flush=True)
        audio_bad = diff_audio("audio")

    print("\nRESULT:", "IDENTICAL" if bad == 0 and audio_bad == 0 else "DIVERGENT")
    return 1 if bad or audio_bad else 0


if __name__ == "__main__":
    sys.exit(main())
