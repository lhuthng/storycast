#!/usr/bin/env python3
"""Diff the audio-reading helpers against the reference.

The fourth gate, and the only one that is not about tensors. `babble_suspect`
counts energy bursts in a waveform, `pause_pad_samples` measures how much silence
two chunks already have between them, and `join_audio_chunks` uses that to top the
pause up. They read *audio*, so a code-level comparison cannot reach them.

Real chunks are generated first — real lengths, real breath, real trailing
silence — because the interesting cases are exactly the ones a synthetic signal
does not have: a chunk that already ends in 100 ms of quiet, a one-syllable chunk
that ran to the frame ceiling.

    python3 tools/audio-utils-parity.py [--lines N]
"""
from __future__ import annotations

import argparse
import glob
import json
import pathlib
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
RUST_BIN = ROOT / "rust/target/debug/bm-tts-check"
WORK = pathlib.Path("/tmp/bm-audio-utils")

HF = pathlib.Path.home() / ".cache/huggingface/hub"
BACKBONE_GLOB = str(HF / "models--pnnbao-ump--VieNeu-TTS-v3-Turbo/snapshots/*/onnx_update")
CODEC_GLOB = str(HF / "models--OpenMOSS-Team--MOSS-Audio-Tokenizer-Nano-ONNX/snapshots/*")

SR = 48_000


def make_chunks(texts: list[str]):
    """Real chunks from the reference engine, with the anchor it computes."""
    import numpy as np
    from vieneu._v3_turbo_engine.onnx_runtime_lite import OnnxV3LiteEngine
    from vieneu_utils.phonemize_text import phonemize_text_with_emotions

    engine = OnnxV3LiteEngine(
        onnx_dir=sorted(glob.glob(BACKBONE_GLOB))[-1],
        codec_dir=sorted(glob.glob(CODEC_GLOB))[-1],
    )
    engine.babble_retries = 0
    rng = np.random.default_rng(20260917)
    speaker = rng.standard_normal(192).astype("<f4")
    ref = rng.integers(0, 1024, size=(6, 16)).astype(np.int64)

    WORK.mkdir(parents=True, exist_ok=True)
    out = []
    for i, t in enumerate(texts):
        ph = phonemize_text_with_emotions(t)
        wav = engine.infer(
            phonemes=ph,
            speaker_emb=speaker,
            ref_codes=ref,
            temperature=0.0,
            max_new_frames=300,
            frame_cap=True,
        )
        wav = np.asarray(wav, dtype="<f4")
        path = WORK / f"chunk{i}.f32"
        wav.tofile(path)
        out.append({"wav": str(path), "sr": SR, "phonemes": ph, "wav_arr": wav})
    return out


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--lines", type=int, default=6)
    args = ap.parse_args()

    if not RUST_BIN.is_file():
        sys.exit(f"build it first: cargo build -p bm-tts --bin bm-tts-check ({RUST_BIN})")

    texts = [
        "Không sao.",
        "Hắn cười.",
        "Thật không?",
        "Dịch Phong đánh giá hai nữ, hơi giật mình.",
        "Trước cửa võ quán, không một bóng người.",
        "Thôi vậy! Thanh Sơn lão tổ cũng chỉ đành chấp nhận.",
    ][: args.lines]

    from vieneu_utils.core_utils import (
        babble_suspect,
        count_speech_bursts,
        edge_silence,
        join_audio_chunks,
        pause_pad_samples,
    )

    print(f"{len(texts)} chunks from the reference engine…", flush=True)
    chunks = make_chunks(texts)

    # The ceiling each chunk was generated under, which the guard needs.
    from vieneu_utils.core_utils import max_expected_frames

    for c in chunks:
        c["cap"] = min(300, max_expected_frames(c["phonemes"]))
        c["frames"] = int(round(len(c["wav_arr"]) / (SR / 12.5)))

    # A join over all of them, with the shipped boundary pauses.
    pauses = [0.50] * (len(chunks) - 1)
    join_paths = [c["wav"] for c in chunks]
    want_join = len(join_audio_chunks([c["wav_arr"] for c in chunks], SR, silence_ps=pauses))

    job = {
        "bursts": [{"wav": c["wav"], "sr": SR} for c in chunks],
        "edge": [{"wav": c["wav"], "sr": SR} for c in chunks],
        "babble": [
            {"wav": c["wav"], "sr": SR, "phonemes": c["phonemes"], "cap": c["cap"], "frames": c["frames"]}
            for c in chunks
        ],
        "pad": [
            {"prev": chunks[i]["wav"], "next": chunks[i + 1]["wav"], "sr": SR, "pause": 0.5}
            for i in range(len(chunks) - 1)
        ],
        "join": [{"chunks": join_paths, "pauses": pauses, "sr": SR}],
    }
    (WORK / "job.json").write_text(json.dumps(job))
    got = subprocess.run(
        [str(RUST_BIN), str(WORK / "job.json"), str(WORK / "out.json")],
        capture_output=True,
        text=True,
        timeout=300,
    )
    if got.returncode != 0:
        sys.exit(f"bm-tts-check failed: {got.stderr[-1500:]}")
    have = json.loads((WORK / "out.json").read_text())

    bad = 0

    def check(label: str, want, have_, i: int) -> None:
        nonlocal bad
        if want != have_:
            bad += 1
            print(f"  MISMATCH {label}[{i}]: python {want} vs rust {have_}")

    print("\nbursts (count_speech_bursts)")
    for i, c in enumerate(chunks):
        w = count_speech_bursts(c["wav_arr"], SR)
        check("bursts", w, have["bursts"][i], i)
        print(f"  chunk {i}: {w} bursts, {len(c['wav_arr'])} samples")

    print("\nedge (edge_silence)")
    for i, c in enumerate(chunks):
        w = edge_silence(c["wav_arr"], SR)
        check("edge", list(w), list(have["edge"][i]), i)

    print("\nbabble (babble_suspect)")
    for i, c in enumerate(chunks):
        w = babble_suspect(c["wav_arr"], SR, c["phonemes"], c["cap"], c["frames"])
        check("babble", list(w), list(have["babble"][i]), i)
        print(f"  chunk {i}: suspect={w[0]} syl={w[1]} bursts={w[2]} frames={w[3]}/{c['cap']}")

    print("\npad (pause_pad_samples)")
    for i in range(len(chunks) - 1):
        w = pause_pad_samples(chunks[i]["wav_arr"], chunks[i + 1]["wav_arr"], SR, 0.5)
        check("pad", w, have["pad"][i], i)
        print(f"  {i}->{i + 1}: {w} samples of padding ({w / SR * 1000:.0f} ms)")

    print("\njoin (join_audio_chunks)")
    check("join", want_join, have["join"][0], 0)
    print(f"  {want_join} samples ({want_join / SR:.2f}s) from {len(chunks)} chunks")

    print("\nRESULT:", "IDENTICAL" if bad == 0 else f"{bad} DIVERGENT")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
