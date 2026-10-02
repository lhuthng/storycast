#!/usr/bin/env python3
"""Convert Pocket TTS's SentencePiece `tokenizer.model` into `tokenizer.json`.

The Rust side reads tokenizers with the `tokenizers` crate it already carries
for VieNeu's `tokenizer.json` — and that crate speaks Unigram, not the raw
SentencePiece protobuf. So the conversion happens once, at bake time, and the
bundle ships JSON.

The `.model` is parsed by hand (a ~60-line protobuf reader) rather than by the
`sentencepiece` package: the repo's environments carry no pip, and the wire
format needs only two fields. What is extracted:

* every piece with its log-score, in file order (the id *is* the order);
* the special ids, found **by name** (`<unk>`, `<s>`, `</s>`) rather than by
  trusting TrainerSpec's field numbers from memory;
* whether the 256 `<0xNN>` byte pieces are present (they are, in this model),
  which decides `byte_fallback`.

Normalisation is the one approximation: SentencePiece ships a precompiled
charsmap (nmt_nfkc) this script cannot replay, so the JSON normalizer states
NFKC. For English prose the two agree on essentially everything; the Rust test
that loads this file round-trips a sentence as the gate.

    python3 tools/pocket-tokenizer.py engines/pocket/export/tokenizer.model \
        engines/pocket/models/tokenizer.json
"""
from __future__ import annotations

import json
import re
import sys


def varint(buf: bytes, i: int) -> tuple[int, int]:
    shift = val = 0
    while True:
        b = buf[i]
        i += 1
        val |= (b & 0x7F) << shift
        if not b & 0x80:
            return val, i
        shift += 7


def fields(buf: bytes):
    """Yield (field_number, wire_type, payload) for one message."""
    i = 0
    while i < len(buf):
        tag, i = varint(buf, i)
        num, wire = tag >> 3, tag & 7
        if wire == 0:
            val, i = varint(buf, i)
            yield num, wire, val
        elif wire == 1:
            yield num, wire, buf[i : i + 8]
            i += 8
        elif wire == 2:
            n, i = varint(buf, i)
            yield num, wire, buf[i : i + n]
            i += n
        elif wire == 5:
            yield num, wire, buf[i : i + 4]
            i += 4
        else:
            raise ValueError(f"wire type {wire} at {i}")


def parse_model(data: bytes) -> list[dict]:
    """The `pieces` (field 1) of a SentencePiece ModelProto."""
    pieces = []
    for num, wire, payload in fields(data):
        if num != 1 or wire != 2:
            continue
        piece = score = None
        ptype = 1  # NORMAL
        for f, w, v in fields(payload):
            if f == 1 and w == 2:
                piece = v.decode("utf-8", errors="replace")
            elif f == 2 and w == 5:
                score = struct_unpack(v)
            elif f == 4 and w == 0:
                ptype = v
        if piece is None:
            continue
        pieces.append({"piece": piece, "score": score, "type": ptype})
    return pieces


def struct_unpack(fixed32: bytes) -> float:
    import struct

    return struct.unpack("<f", fixed32)[0]


def main() -> int:
    src, dst = sys.argv[1], sys.argv[2]
    raw = open(src, "rb").read()
    pieces = parse_model(raw)

    def find(name: str) -> int | None:
        for i, p in enumerate(pieces):
            if p["piece"] == name:
                return i
        return None

    unk = find("<unk>")
    bos = find("<s>")
    eos = find("</s>")
    # By name, not by the type field: the byte pieces are `<0x00>`…`<0xFF>`
    # and counting them is the fact byte_fallback needs.
    byte_pieces = [p for p in pieces if re.fullmatch(r"<0x[0-9A-Fa-f]{2}>", p["piece"])]
    if unk is None:
        sys.exit("no <unk> piece — is this a SentencePiece model?")
    vocab = [[p["piece"], p["score"] if p["score"] is not None else 0.0] for p in pieces]

    out = {
        "version": "1.0",
        "truncation": None,
        "padding": None,
        "added_tokens": [
            {"id": i, "content": p["piece"], "single_word": False, "lstrip": False,
             "rstrip": False, "normalized": False, "special": p["type"] in (2, 3)}
            for i, p in enumerate(pieces) if p["type"] in (2, 3)
        ],
        "normalizer": {"type": "NFKC"},
        "pre_tokenizer": {
            "type": "Metaspace",
            "replacement": "▁",
            "prepend_scheme": "always",
        },
        "post_processor": None,
        "decoder": {
            "type": "Sequence",
            "decoders": [
                {"type": "Replace", "pattern": {"String": "▁"}, "content": " "},
                {"type": "ByteFallback"},
                {"type": "Fuse"},
            ],
        },
        "model": {
            "type": "Unigram",
            "unk_id": unk,
            "bos_id": bos,
            "eos_id": eos,
            "byte_fallback": len(byte_pieces) >= 256,
            "vocab": vocab,
        },
    }
    with open(dst, "w", encoding="utf-8") as f:
        json.dump(out, f, ensure_ascii=False)
    print(
        f"{len(vocab)} pieces -> {dst} "
        f"(unk {unk}, bos {bos}, eos {eos}, byte_fallback {len(byte_pieces) >= 256})"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
