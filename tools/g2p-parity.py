#!/usr/bin/env python3
"""Diff the Rust G2P against the Python wheel on the same corpus.

The second gate of the "no Python" port. ONNX was proven bit-identical by
`bm-tts-probe`; this proves the text front end is too. Phonemes drift silently
— nothing downstream can tell a wrong phoneme from a right one — so this is the
one place the port can be checked before audio exists to listen to.

    python3 tools/g2p-parity.py                 # wheel is the reference
    python3 tools/g2p-parity.py --record f.txt  # freeze today's output instead

The corpus is real chapter prose (the thing the pipeline actually eats) plus
the cases a normalizer exists for: numbers, dates, units, money, ranges, English
code-switching, and the punctuation shapes `punc_norm` cares about. Chapter
prose alone does not exercise half of the seventeen stages.

Requires the Python wheel only while it is still installed; after the port lands
use `--record` to keep a frozen corpus and compare future changes against that.
"""
from __future__ import annotations

import argparse
import glob
import os
import pathlib
import random
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
BIN = ROOT / "rust/target/debug/bm-tts-g2p"

# Where the dictionary is, in order of preference: the bake output, then the
# vendored crate's upstream layout, then the installed wheel. All three are the
# same 62,829,820 bytes (see rust/vendor/sea-g2p/VENDORED.md for the hash).
DICT_CANDIDATES = [
    ROOT / "models/sea_g2p.bin",
    ROOT / "rust/vendor/sea-g2p/python/sea_g2p/sea_g2p.bin",
    ROOT / ".venv/lib/python3.12/site-packages/sea_g2p/sea_g2p.bin",
]


def find_dict(explicit: str | None) -> pathlib.Path:
    if explicit:
        p = pathlib.Path(explicit)
        if not p.is_file():
            sys.exit(f"no dictionary at {p}")
        return p
    for p in DICT_CANDIDATES:
        if p.is_file():
            return p
    sys.exit("no sea_g2p.bin found; pass --dict, or run the bake step")


def corpus() -> list[str]:
    random.seed(20260917)  # the same corpus every run, or the diff means nothing
    lines: list[str] = []
    chapters = sorted(glob.glob(str(ROOT / "data/chapters/*.txt")))
    for f in chapters[:: max(1, len(chapters) // 40)][:40]:
        with open(f, encoding="utf-8") as fh:
            body = [l.strip() for l in fh if 20 < len(l.strip()) < 220]
        lines.extend(random.sample(body, min(6, len(body))))

    lines += [
        "Năm 1995, hắn 27 tuổi, cao 1m75 và nặng 68,5 kg.",
        "Giá là 1.250.000 đồng, giảm 15% so với hôm qua.",
        "Ngày 3/2/1930 là một mốc lịch sử.",
        "Lúc 7h30 sáng, tàu khởi hành từ ga Hà Nội.",
        "Diện tích khoảng 331.212 km2, dân số 96,2 triệu người.",
        "Anh ấy nói OK, fine, no problem luôn.",
        "Tọa độ là 21°01'B 105°51'Đ.",
        "Số điện thoại 0912 345 678 và email test@example.com.",
        "Xem tại https://example.com/bai-viet/123 nhé.",
        "Công thức: E = mc2, và ln(x) > 0.",
        "Từ 10-20 người, tức khoảng 1/3 tổng số.",
        "Chương 37: Tốt một câu thon thả thục nữ, quân tử hảo cầu",
        "Hắn cười. Cô ấy khóc!",
        "Không sao",
        "Thật không?",
        "Vâng, đúng vậy…",
        "Ông ấy nói: “Đi đi.”",
        "Đường Nguyễn Huệ, quận 1, TP.HCM.",
        "Nhiệt độ -5°C vào mùa đông.",
        "1 + 1 = 2 và 3 x 4 = 12.",
    ]
    return [l for l in lines if l.strip()]


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--dict", help="path to sea_g2p.bin")
    ap.add_argument("--record", help="write the Rust output here and stop")
    args = ap.parse_args()

    if not BIN.is_file():
        sys.exit(f"build it first: cargo build -p bm-tts --bin bm-tts-g2p ({BIN})")
    d = find_dict(args.dict)
    lines = corpus()

    rust = subprocess.run(
        [str(BIN), str(d), "punc"],
        input="\n".join(lines) + "\n",
        capture_output=True,
        text=True,
        timeout=900,
    )
    if rust.returncode != 0:
        sys.exit(f"bm-tts-g2p failed ({rust.returncode}): {rust.stderr[:800]}")
    rust_lines = rust.stdout.rstrip("\n").split("\n")

    if args.record:
        pathlib.Path(args.record).write_text("\n".join(rust_lines) + "\n")
        print(f"recorded {len(rust_lines)} lines to {args.record}")
        return 0

    try:
        from sea_g2p import SEAPipeline
    except ImportError:
        sys.exit(
            "the Python wheel is not installed and no --record baseline was given;\n"
            "install sea-g2p==0.9.1 or run with --record to freeze a baseline"
        )
    pipe = SEAPipeline(lang="vi")
    py_lines = [pipe.run(l, punc_norm=True) for l in lines]

    if len(rust_lines) != len(py_lines):
        sys.exit(f"line count mismatch: rust {len(rust_lines)} vs python {len(py_lines)}")

    bad = 0
    for i, (src, r, p) in enumerate(zip(lines, rust_lines, py_lines)):
        if r != p:
            bad += 1
            if bad <= 5:
                print(f"\n--- MISMATCH line {i}\n    src:    {src[:110]}")
                print(f"    rust:   {r[:180]}\n    python: {p[:180]}")
    print(f"\n{len(lines)} lines, {len(lines) - bad} identical, {bad} differing")
    print("RESULT:", "IDENTICAL" if bad == 0 else "DIVERGENT")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
