#!/usr/bin/env python3
"""Diff the Rust text layer against the reference, chunk by chunk.

The fifth gate. The text front end is where a wrong answer is quietest: a
mis-split sentence, a chunk boundary in the wrong place, or a lost emotion cue
all produce *plausible* audio that reads the wrong words. So this compares the
three stages separately rather than only the final phonemes:

* `split_into_sentences` — on the raw text, quote- and bracket-aware
* `normalize_to_chunks_v3_with_gaps` — the `(chunks, gaps)` pair the render uses
* `phonemize_text_with_emotions` — per chunk, cues preserved

The corpus is real chapter paragraphs plus the cases each rule exists for:
quotes containing `?`, decimals and clock times that must not split, unbalanced
quotes, short chunks that must merge, emotion cues, and text long enough to force
a mid-sentence cut.

    python3 tools/text-parity.py [--paras N]
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
RUST_BIN = ROOT / "rust/target/debug/bm-tts-check"
WORK = pathlib.Path("/tmp/bm-text-parity")

DICT_CANDIDATES = [
    ROOT / "models/sea_g2p.bin",
    ROOT / "rust/vendor/sea-g2p/python/sea_g2p/sea_g2p.bin",
    ROOT / ".venv/lib/python3.12/site-packages/sea_g2p/sea_g2p.bin",
]

MAX_CHARS = 256
MIN_CHUNK_CHARS = 20


def find_dict() -> pathlib.Path:
    for p in DICT_CANDIDATES:
        if p.is_file():
            return p
    sys.exit("no sea_g2p.bin found; run the bake step or install sea-g2p")


def corpus(limit: int) -> list[str]:
    random.seed(20260917)
    chapters = sorted(glob.glob(str(ROOT / "data/chapters/*.txt")))
    out: list[str] = []
    for f in chapters[:: max(1, len(chapters) // 25)][:25]:
        with open(f, encoding="utf-8") as fh:
            paras = [l.strip() for l in fh if 40 < len(l.strip()) < 900]
        if paras:
            out.extend(random.sample(paras, min(2, len(paras))))

    out += [
        # A question inside a quotation is not a sentence end.
        'Có phải kiểu như: "Rồi sao nữa? Mình phải làm đến bao giờ?", đúng không anh?',
        # Decimals and clock times must survive.
        "Giá là 4.200,5 điểm. Hẹn lúc 8.30 sáng nhé.",
        # An unbalanced quote must not swallow the rest.
        'Ông ấy nói: "Đi đi. Tôi đứng dậy. Rồi bỏ đi.',
        # Emotion cues.
        "Hắn [cười] rồi nói tiếp.",
        "[thở dài] Thôi vậy.",
        "Nàng đáp [hắng giọng] một tiếng.",
        # Short chunks that must merge, and a heading glued to its body.
        "Chương một.",
        "Chương một.\nNội dung dài hơn hẳn để đoạn sau không bị coi là ngắn.",
        # Paragraph boundaries.
        "Đoạn thứ nhất ở đây.\n\nĐoạn thứ hai ở đây.",
        # Long enough to force a mid-sentence cut, with connectors to cut before.
        "Trong lúc đó, hắn vẫn không ngừng suy nghĩ về những gì đã xảy ra, "
        "và rồi hắn nhận ra rằng mọi chuyện đều bắt nguồn từ một câu nói nhỏ, "
        "nhưng câu nói ấy lại có sức nặng khủng khiếp, cho nên hắn quyết định "
        "sẽ không bao giờ nhắc lại chuyện đó với bất kỳ ai nữa, kể cả người "
        "thân thiết nhất, vì hắn biết rằng nó sẽ chỉ mang lại đau lòng mà thôi.",
    ]
    return [p for p in out if p.strip()][:limit]


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--paras", type=int, default=40)
    args = ap.parse_args()

    if not RUST_BIN.is_file():
        sys.exit(f"build it first: cargo build -p bm-tts --bin bm-tts-check ({RUST_BIN})")
    d = find_dict()

    from vieneu_utils.core_utils import split_into_sentences
    from vieneu_utils.phonemize_text import (
        normalize_to_chunks_v3_with_gaps,
        phonemize_text_with_emotions,
    )

    paras = corpus(args.paras)
    WORK.mkdir(parents=True, exist_ok=True)
    job = {
        "sentences": paras,
        "text": [
            {"text": p, "max_chars": MAX_CHARS, "min_chunk_chars": MIN_CHUNK_CHARS}
            for p in paras
        ],
    }
    (WORK / "job.json").write_text(json.dumps(job))
    got = subprocess.run(
        [str(RUST_BIN), str(WORK / "job.json"), str(WORK / "out.json"), "--dict", str(d)],
        capture_output=True,
        text=True,
        timeout=1800,
    )
    if got.returncode != 0:
        sys.exit(f"bm-tts-check failed: {got.stderr[-1500:]}")
    have = json.loads((WORK / "out.json").read_text())

    bad = 0

    print(f"split_into_sentences on {len(paras)} paragraphs")
    for i, p in enumerate(paras):
        want = split_into_sentences(p)
        if want != have["sentences"][i]:
            bad += 1
            if bad <= 3:
                print(f"  MISMATCH [{i}] {p[:60]!r}")
                print(f"    python {want}")
                print(f"    rust   {have['sentences'][i]}")

    print(f"\nchunks + gaps on {len(paras)} paragraphs")
    chunk_bad = 0
    for i, p in enumerate(paras):
        want_chunks, want_gaps = normalize_to_chunks_v3_with_gaps(
            p, max_chars=MAX_CHARS, min_chunk_chars=MIN_CHUNK_CHARS
        )
        r = have["text"][i]
        if want_chunks != r["chunks"] or want_gaps != r["gaps"]:
            bad += 1
            chunk_bad += 1
            if chunk_bad <= 3:
                print(f"  MISMATCH [{i}] {p[:60]!r}")
                if want_chunks != r["chunks"]:
                    print(f"    chunks python ({len(want_chunks)}): {want_chunks}")
                    print(f"    chunks rust   ({len(r['chunks'])}): {r['chunks']}")
                if want_gaps != r["gaps"]:
                    print(f"    gaps python {want_gaps}")
                    print(f"    gaps rust   {r['gaps']}")
    total_chunks = sum(len(h["chunks"]) for h in have["text"])
    print(f"  {total_chunks} chunks across {len(paras)} paragraphs, {chunk_bad} paragraphs differing")

    print(f"\nphonemize_text_with_emotions on {total_chunks} chunks")
    phon_bad = 0
    for i, p in enumerate(paras):
        r = have["text"][i]
        for j, ch in enumerate(r["chunks"]):
            want = phonemize_text_with_emotions(ch)
            if want != r["phonemes"][j]:
                bad += 1
                phon_bad += 1
                if phon_bad <= 3:
                    print(f"  MISMATCH [{i}.{j}] {ch[:60]!r}")
                    print(f"    python {want[:110]}")
                    print(f"    rust   {r['phonemes'][j][:110]}")
    print(f"  {total_chunks - phon_bad}/{total_chunks} identical")

    print("\nRESULT:", "IDENTICAL" if bad == 0 else f"{bad} DIVERGENT")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
