#!/usr/bin/env python3
"""Audit a workspace's scripts against their source chapters.

The digest source gate (`validate_source_alignment` in
`rust/crates/bm-core/src/digest.rs`) refuses a chapter where a character speaks
a source span that is not quoted dialogue — "narration must be Narrator". The
scripts written *before* that gate existed were never checked, so this walks
them and reports exactly what the gate would now reject.

Three signatures, all from the same root cause — the model reading a mention as
a speaker:

  mention
      HIGH PRECISION. A non-Narrator segment whose text opens with the
      speaker's OWN canonical name in the third person: `Dịch Phong` speaking
      "Dịch Phong tức giận trừng Ngao Khánh một cái". The character was only
      *mentioned*, and this needs no source-quote evidence to be wrong. Name
      only — a pronoun or a vocative at the head of a line is legitimate
      dialogue and is deliberately not flagged.

  narration_spoken_by_character
      A non-Narrator segment whose text maps to source prose outside quotes.
      Wider net than `mention`, and only as trustworthy as the source's quoting:
      a crawl that dropped or shifted its `"` markers reads as damaged prose and
      produces false hits. Cross-check a chapter before believing it.

  quote_marker_in_text
      A segment whose text still carries `"`/`“`/`”`. The gate forbids these
      outright: the quote delimiter belongs to the split, not to the span.

Report-only. Writes nothing. Matches the source with the same quote-delimiter
rule the Rust preparer uses, exactly once per event, and prints the fraction of
each mapped span that sits inside quotes so borderline lines can be eyeballed.

Usage:
  tools/attribution-audit.py workspaces/beyond-myriads/data
  tools/attribution-audit.py workspaces/beyond-myriads/data --chapter 16
  tools/attribution-audit.py workspaces/beyond-myriads/data --all
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path

# The preparer's delimiters, verbatim: straight quotes open and close, the two
# smart pairs are directional. Everything else is prose.
OPENERS = {"\u0022", "\u201c", "\u300c"}
CLOSERS = {"\u0022", "\u201d", "\u300d"}

TAG = re.compile(r"\[[^\]]*\]")


def quoted_mask(text: str) -> list[bool]:
    """True at every offset that sits inside a quoted run."""
    mask = [False] * len(text)
    depth = 0
    for i, ch in enumerate(text):
        if ch in OPENERS and depth == 0:
            depth = 1
            mask[i] = True
        elif ch in CLOSERS and depth == 1:
            depth = 0
            mask[i] = True
        elif depth:
            mask[i] = True
    return mask


def squeeze(s: str) -> str:
    return re.sub(r"\s+", " ", s).strip()


def normalized(text: str) -> tuple[str, list[int]]:
    """Collapse whitespace, keeping a map back to original offsets."""
    out: list[str] = []
    back: list[int] = []
    prev_space = False
    for i, ch in enumerate(text):
        if ch.isspace():
            if prev_space:
                continue
            out.append(" ")
            back.append(i)
            prev_space = True
        else:
            out.append(ch)
            back.append(i)
            prev_space = False
    return "".join(out), back


def locate(needle: str, hay_norm: str, hay_back: list[int], start: int) -> tuple[int, int] | None:
    """Offset range of `needle` in the original text, searching from `start`."""
    if not needle:
        return None
    at = hay_norm.find(needle, start)
    if at < 0:
        return None
    lo = hay_back[at]
    hi = hay_back[min(at + len(needle) - 1, len(hay_back) - 1)] + 1
    return lo, hi


def apply_fixes(source: str, fixes: list) -> str:
    for f in fixes or []:
        before = f.get("before")
        after = f.get("after")
        if before and after:
            source = source.replace(before, after)
    return source


def audit_chapter(data_dir: Path, n: int) -> dict:
    chapter_path = data_dir / "chapters" / f"ch{n:02d}.txt"
    script_path = data_dir / f"script-{n:02d}.json"
    if not chapter_path.exists() or not script_path.exists():
        return {"chapter": n, "missing": True}

    source = chapter_path.read_text(encoding="utf-8")
    script = json.loads(script_path.read_text(encoding="utf-8"))
    source = apply_fixes(source, script.get("fixes"))
    mask = quoted_mask(source)
    hay, back = normalized(source)

    cursor = 0
    findings: list[dict] = []
    for idx, seg in enumerate(script.get("segments", [])):
        if "speaker" not in seg:
            continue  # a sound item
        speaker = seg.get("speaker", "")
        raw = seg.get("text", "")
        cleaned_early = squeeze(TAG.sub("", raw))
        # Third-person self-reference: the line is `Name <predicate>`, so the
        # named character is only the subject of the prose. Case-insensitive
        # because the source sometimes lowercases a leading name.
        if speaker != "Narrator" and cleaned_early.lower().startswith(speaker.lower()):
            rest = cleaned_early[len(speaker) :]
            if rest[:1] in ("", " ") and rest.strip(" .,!…"):
                findings.append(
                    {
                        "kind": "mention",
                        "segment": idx,
                        "speaker": speaker,
                        "text": cleaned_early[:120],
                    }
                )
        if any(c in raw for c in ('"', "\u201c", "\u201d")):
            findings.append(
                {
                    "kind": "quote_marker_in_text",
                    "segment": idx,
                    "speaker": speaker,
                    "text": squeeze(raw)[:120],
                }
            )
        cleaned = squeeze(TAG.sub("", raw))
        span = locate(cleaned, hay, back, cursor)
        if span is None:
            # A typo fix or a trim can break the exact match; retry from the
            # chapter start on a long prefix before giving up.
            prefix = cleaned[:60]
            span = locate(prefix, hay, back, 0) if len(prefix) >= 20 else None
        if span is None:
            findings.append(
                {
                    "kind": "unmapped",
                    "segment": idx,
                    "speaker": speaker,
                    "text": cleaned[:120],
                }
            )
            continue
        lo, hi = span
        cursor = lo
        inside = sum(mask[lo:hi])
        frac = inside / max(hi - lo, 1)
        if speaker != "Narrator" and frac < 0.5:
            findings.append(
                {
                    "kind": "narration_spoken_by_character",
                    "segment": idx,
                    "speaker": speaker,
                    "quoted_fraction": round(frac, 2),
                    "text": cleaned[:120],
                }
            )
        elif speaker == "Narrator" and frac > 0.95:
            findings.append(
                {
                    "kind": "dialogue_spoken_by_narrator",
                    "segment": idx,
                    "speaker": speaker,
                    "quoted_fraction": round(frac, 2),
                    "text": cleaned[:120],
                }
            )
    return {"chapter": n, "findings": findings}


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("data_dir", type=Path, help="a workspace's data/ directory")
    ap.add_argument("--chapter", type=int, action="append", default=None)
    ap.add_argument("--all", action="store_true", help="print every finding, not a summary")
    ap.add_argument(
        "--kind",
        action="append",
        default=None,
        help="only count this signature (repeatable)",
    )
    args = ap.parse_args()

    chapters = (
        args.chapter
        if args.chapter
        else sorted(
            int(p.stem[2:]) for p in (args.data_dir / "chapters").glob("ch*.txt")
        )
    )

    results = []
    for n in chapters:
        r = audit_chapter(args.data_dir, n)
        if r.get("missing"):
            continue
        hits = r["findings"]
        if args.kind:
            hits = [h for h in hits if h["kind"] in args.kind]
            r["findings"] = hits
        if hits:
            results.append(r)

    results.sort(key=lambda r: -len(r["findings"]))
    total = sum(len(r["findings"]) for r in results)
    print(f"{len(results)} of {len(chapters)} chapters carry findings ({total} total)")
    for r in results:
        counts: dict[str, int] = {}
        for h in r["findings"]:
            counts[h["kind"]] = counts.get(h["kind"], 0) + 1
        summary = ", ".join(f"{k}×{v}" for k, v in sorted(counts.items()))
        print(f"  ch{r['chapter']:<3} {len(r['findings']):>3}  {summary}")
        if args.all:
            for h in r["findings"]:
                q = h.get("quoted_fraction")
                qs = f" q={q}" if q is not None else ""
                print(f"        [{h['kind']}]{qs} {h['speaker']}: {h['text']}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
