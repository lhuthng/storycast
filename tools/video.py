#!/usr/bin/env python3
"""Render a Storycast book's acts into one video from the cue sidecars.

The merge publishes `output/Ch.N - Title.mp3` and, beside it,
`Ch.N - Title.cues.json` — every line's interval on the delivered clock. This
tool reads those, cuts them into readable captions, and lays the acts out on the
approved template (paper ground, a swapping act title, a bar with one segment
per act and a spinning thumb) over the chapter audio.

Subtitles are drawn into the frame from the same captions that ship as `.srt`
and `.vtt` beside the video.

    tools/video.py --workspace workspaces/beyond-myriads --acts acts.json

The acts manifest names the grouping, not the presentation:

    { "acts": [ { "act": 1, "title": "…", "chapters": [1, 2] },
                { "act": 2, "title": "…", "chapters": [3] } ] }

ffmpeg on this box has no drawtext/libass, so text and art are rasterised with
Pillow and composited: the act title and the captions each become a timed strip
of images ffmpeg overlays, and the bar is a `drawbox` per act.
"""

from __future__ import annotations

import argparse
import io
import json
import math
import re
import shutil
import subprocess
import sys
from pathlib import Path

import numpy as np
from PIL import Image, ImageChops, ImageDraw, ImageFont

ROOT = Path(__file__).resolve().parent.parent


# --------------------------------------------------------------------------- #
# colour + template
# --------------------------------------------------------------------------- #


def parse_color(s: str) -> tuple[int, int, int, int]:
    s = s.strip()
    m = re.fullmatch(r"#([0-9a-fA-F]{6})", s)
    if m:
        v = int(m.group(1), 16)
        return ((v >> 16) & 255, (v >> 8) & 255, v & 255, 255)
    m = re.fullmatch(r"rgba?\(([^)]*)\)", s)
    if m:
        parts = [p.strip() for p in m.group(1).split(",")]
        r, g, b = (int(float(p)) for p in parts[:3])
        a = float(parts[3]) if len(parts) > 3 else 1.0
        return (r, g, b, int(round(a * 255)))
    raise ValueError(f"cannot parse colour {s!r}")


def rgb_hex(c: tuple[int, int, int, int]) -> str:
    return "%02x%02x%02x" % c[:3]


def parse_radial(s: str) -> dict:
    body = re.fullmatch(r"radial-gradient\((.*)\)", s.strip(), re.S).group(1)
    geometry, rest = body.split(" at ", 1)
    rx, ry = (float(t.rstrip("%")) / 100.0 for t in geometry.split())
    centre, stop_text = rest.split(",", 1)
    cx, cy = (float(t.rstrip("%")) / 100.0 for t in centre.split())
    stops = []
    for part in stop_text.split(","):
        col, pct = part.strip().split()
        stops.append((float(pct.rstrip("%")) / 100.0, parse_color(col)))
    return {"cx": cx, "cy": cy, "rx": rx, "ry": ry, "stops": stops}


def load_template(path: Path) -> dict:
    template = json.loads(Path(path).read_text(encoding="utf-8"))
    for role, rel in template.get("font_files", {}).items():
        p = Path(rel)
        candidate = p if p.is_absolute() else ROOT / rel
        if not candidate.exists():
            sys.exit(f"template font_files.{role}: no such file {candidate}")
        template["font_files"][role] = str(candidate)
    return template


# --------------------------------------------------------------------------- #
# layout: the mock's box model, in pixels
# --------------------------------------------------------------------------- #


def anchor_hv(anchor: str) -> tuple[float, float]:
    h = v = None
    for t in anchor.split("-"):
        if t == "left":
            h = 0.0
        elif t == "right":
            h = 1.0
        elif t == "top":
            v = 0.0
        elif t == "bottom":
            v = 1.0
    return (0.5 if h is None else h, 0.5 if v is None else v)


def zone_boxes(template: dict, W: int, H: int) -> dict:
    cw, ch = template["canvas"]["size"]
    boxes: dict[str, dict] = {}
    for name, z in template["zones"].items():
        h, v = anchor_hv(z["anchor"])
        w, ht = z["size"]
        if z.get("aspect"):
            ht = (w * cw) / (z["aspect"] * ch)
        box = {
            "left": z["at"][0] * W - w * W * h,
            "top": z["at"][1] * H - ht * H * v,
            "width": w * W,
            "height": ht * H,
        }
        if z.get("align_top") and z["align_top"] in boxes:
            box["top"] = boxes[z["align_top"]]["top"]
        if z.get("align_bottom") and z["align_bottom"] in boxes:
            r = boxes[z["align_bottom"]]
            box["top"] = r["top"] + r["height"] - box["height"]
        boxes[name] = box
    return boxes


# --------------------------------------------------------------------------- #
# rasterising: the paper ground and the act title
# --------------------------------------------------------------------------- #


def paper(template: dict, W: int, H: int) -> Image.Image:
    grad = parse_radial(template["palette"]["background"])
    ys, xs = np.mgrid[0:H, 0:W]
    d = np.sqrt(
        ((xs / W - grad["cx"]) / grad["rx"]) ** 2
        + ((ys / H - grad["cy"]) / grad["ry"]) ** 2
    )
    d = np.clip(d, 0.0, 1.0)
    out = np.zeros((H, W, 3), np.float64)
    stops = grad["stops"]
    for (p0, c0), (p1, c1) in zip(stops, stops[1:]):
        m = (d >= p0) & (d <= p1)
        if not m.any():
            continue
        t = ((d[m] - p0) / (p1 - p0))[:, None]
        out[m] = np.array(c0[:3]) * (1 - t) + np.array(c1[:3]) * t
    # the mock's inset shadow, as a soft vignette toward the frame edges
    edge = np.clip(np.sqrt(((xs / W - 0.5) * 2) ** 2 + ((ys / H - 0.5) * 2) ** 2) / 1.414, 0, 1)
    a = 0.05 * edge**2
    out = out * (1 - a[..., None])
    return Image.fromarray(out.astype(np.uint8), "RGB")


def wrap_to_width(text: str, font: ImageFont.FreeTypeFont, draw: ImageDraw.ImageDraw,
                  max_width: float, max_lines: int = 2) -> list[str]:
    words = text.split()
    lines: list[str] = []
    cur = ""
    for w in words:
        cand = f"{cur} {w}".strip()
        if cur and draw.textlength(cand, font=font) > max_width:
            lines.append(cur)
            cur = w
        else:
            cur = cand
    if cur:
        lines.append(cur)
    if len(lines) > max_lines:  # last line absorbs the overflow; the frame clips it
        lines = lines[: max_lines - 1] + [" ".join(lines[max_lines - 1:])]
    return lines


def act_title_plate(template: dict, boxes: dict, act: dict, static: Image.Image,
                    W: int, parts: Path, i: int) -> Path:
    """The act-title zone as an opaque tile: background crop + label + title."""
    box = boxes["act_title"]
    x0, y0 = round(box["left"]), round(box["top"])
    w = round(box["width"]) & ~1
    h = round(box["height"]) & ~1
    tile = static.crop((x0, y0, x0 + w, y0 + h)).convert("RGB")

    draw = ImageDraw.Draw(tile)
    unit = W * template["type"]["unit"] / 100.0
    label_font = ImageFont.truetype(template["font_files"]["act_label"],
                                    round(unit * template["type"]["act_label"]))
    title_font = ImageFont.truetype(template["font_files"]["act_title"],
                                    round(unit * template["type"]["act_title"]))
    pad = round(W * 0.024)
    x = pad
    y = 0
    label = template["act_title"]["label"].replace("#", str(act["act"]))
    draw.text((x, y), label, font=label_font,
              fill=parse_color(template["palette"]["act_label"]))
    label_h = unit * template["type"]["act_label"] * 1.15
    ty = y + label_h + 0.1 * unit * template["type"]["act_label"]
    lines = wrap_to_width(act["title"], title_font, draw, w - 2 * pad, max_lines=2)
    lh = unit * template["type"]["act_title"] * 1.06
    for k, line in enumerate(lines):
        draw.text((x, ty + k * lh), line, font=title_font,
                  fill=parse_color(template["palette"]["act_title"]))

    path = parts / f"title-{i:02d}.png"
    tile.save(path)
    return path


# --------------------------------------------------------------------------- #
# the timeline: one segment per act
# --------------------------------------------------------------------------- #


def drawbox(x, y, w, h, colour, alpha=None, enable=None) -> str:
    c = rgb_hex(colour)
    a = "" if alpha is None else f"@{alpha:.3f}"
    s = f"drawbox=x={x:.2f}:y={y:.2f}:w={w:.2f}:h={h:.2f}:color=0x{c}{a}:t=fill:replace=1"
    if enable:
        s += f":enable='{enable}'"
    return s


def timeline_geometry(template: dict, durs: list[float], W: int, H: int) -> dict:
    tz = zone_boxes(template, W, H)["timeline"]
    ch = template["canvas"]["size"][1]
    track_h = round(H * template["timeline"]["track"]["thickness"] / ch)
    gap = round(H * 0.008)
    total = sum(durs) or 1.0
    usable = tz["width"] - gap * (len(durs) - 1)
    x, segs = tz["left"], []
    for dur in durs:
        w = usable * dur / total
        segs.append((x, w))
        x += w + gap
    return {"left": tz["left"], "width": tz["width"], "track_h": track_h,
            "cy": tz["top"] + tz["height"] / 2, "segs": segs}


def thumb_movie(template: dict, parts: Path, fps: int, spin: bool,
                art: Image.Image) -> Path:
    """One revolution (or a single frame) as a loopable alpha clip."""
    src = parts / "thumb.png"
    art.save(src)
    out = parts / "thumb.mov"
    th = template["timeline"]["thumb"]
    spin_s = th.get("spin_s", 1.2)
    sign = "-" if re.match(r"^(ccw|reverse|back)$", th.get("direction", ""), re.I) else ""
    frames = max(1, round(spin_s * fps)) if spin else 1
    vf = ["-vf", f"rotate='{sign}2*PI*t/{spin_s}':fillcolor=none"] if spin else []
    subprocess.run(["ffmpeg", "-y", "-hide_banner", "-loglevel", "error", "-nostdin",
                    "-loop", "1", "-framerate", str(fps), "-i", str(src),
                    *vf, "-frames:v", str(frames), "-c:v", "qtrle", str(out)], check=True)
    return out


def thumb_image(template: dict, H: int) -> tuple[Image.Image | None, bool]:
    """The moving thumb: the illustration, round-clipped, or the plain disc."""
    thumb = template.get("timeline", {}).get("thumb")
    if not thumb:
        return None, False
    dia = round(H * thumb["radius"] * 2 * thumb.get("scale", 1) / template["canvas"]["size"][1])
    rel = thumb.get("single") or thumb.get("sheet")
    if rel:
        p = Path(rel)
        p = p if p.is_absolute() else ROOT / rel
        if not p.exists():
            sys.exit(f"timeline.thumb.single: no such file {p}")
        art = Image.open(p).convert("RGBA").resize((dia, dia), Image.LANCZOS)
        mask = Image.new("L", (dia * 4, dia * 4), 0)
        ImageDraw.Draw(mask).ellipse([0, 0, dia * 4 - 1, dia * 4 - 1], fill=255)
        art.putalpha(ImageChops.multiply(art.getchannel("A"),
                                         mask.resize((dia, dia), Image.LANCZOS)))
        return art, True
    fill = parse_color(template["palette"]["thumb"])
    ring = parse_color(template["palette"]["thumb_ring"])
    S = dia * 4
    im = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    d = ImageDraw.Draw(im)
    d.ellipse([0, 0, S - 1, S - 1], fill=ring)
    inner = S * 0.68
    off = (S - inner) / 2
    d.ellipse([off, off, off + inner - 1, off + inner - 1], fill=fill)
    return im.resize((dia, dia), Image.LANCZOS), False


# --------------------------------------------------------------------------- #
# captions
# --------------------------------------------------------------------------- #

# A dash is a pause the narrator takes, not a word: on screen it reads as one.
DASHES = re.compile(r"[—–―]+")
SENTENCE_END = ".!?…。！？；;"
# Only these speak without naming themselves.
NARRATOR = {"narrator", "người dẫn chuyện", "người dẫn", "dẫn chuyện"}


def caption_text(text: str) -> str:
    """The sidecar's line, as it should be read on screen."""
    out = DASHES.sub("; ", text)
    # A dash after a full stop is just the next sentence starting.
    out = re.sub(r"([.!?…。！？])\s*;\s*", r"\1 ", out)
    out = re.sub(r"\s*;\s*", "; ", out).strip(" ;")
    return re.sub(r"\s{2,}", " ", out)


def speaker_label(speaker: str) -> str:
    """`"` for anyone but the narrator — their sticker carries the name."""
    name = speaker.strip()
    return "" if not name or name.casefold() in NARRATOR else '"'


# --------------------------------------------------------------------------- #
# speaker stickers: the portrait that talks instead of the name
# --------------------------------------------------------------------------- #


def sticker_config(template: dict) -> dict:
    return template.get("speaker_sticker", {})


def sticker_for(speaker: str, cfg: dict) -> Path | None:
    """The portrait for a speaker, or None for the narrator.

    Matched case-insensitively: the script spells a name one way and the file
    or the config key another, and a silent fallback to the anonymous portrait
    is worse than a case difference.
    """
    name = speaker.strip()
    if not cfg or not name or name.casefold() in NARRATOR:
        return None
    wanted = name.casefold()
    rel = next((v for k, v in cfg.get("speakers", {}).items()
                if k.strip().casefold() == wanted), None) or cfg.get("fallback")
    if not rel:
        return None
    p = Path(rel)
    p = p if p.is_absolute() else ROOT / rel
    if not p.exists():
        sys.exit(f"speaker_sticker: no such file {p}")
    return p


def sticker_art(path: Path) -> Image.Image:
    """The portrait, trimmed to its own pixels: the transparent margin is not
    part of the character, and sizing by it would leave a gap beside the text."""
    art = Image.open(path).convert("RGBA")
    box = art.getchannel("A").getbbox()
    return art.crop(box) if box else art


def sticker_ratio(template: dict) -> float:
    """How far the timeline thumb shrinks its own image — the sticker's scale too.

    The thumb resizes `timeline.thumb.single` to `radius * scale * 2`; the
    sticker takes the same ratio of its own width, so a portrait is never fitted
    into a fixed box and keeps its proportions.
    """
    thumb = template.get("timeline", {}).get("thumb") or {}
    rel = thumb.get("single") or thumb.get("sheet")
    if not thumb or not rel:
        return 1.0
    src = Path(rel)
    src = src if src.is_absolute() else ROOT / rel
    natural = Image.open(src).width if src.exists() else 1
    H = template["canvas"]["size"][1]
    dia = H * thumb["radius"] * 2 * thumb.get("scale", 1) / H
    return dia / max(natural, 1)


def sticker_box(template: dict, cfg: dict, speakers: set[str], W: int) -> tuple[int, int]:
    """The strip every sticker is drawn into: the widest portrait, plus the
    vertical slack squash-and-stretch needs. One box for all, so the badge is
    the same size whichever speaker is talking."""
    react = cfg.get("react", {})
    ratio = sticker_ratio(template) * cfg.get("zoom", 1.0)
    widest = tallest = 1.0
    for sp in speakers:
        p = sticker_for(sp, cfg)
        if p is None:
            continue
        im = sticker_art(p)
        widest = max(widest, im.width * ratio)
        tallest = max(tallest, im.height * ratio)
    pad = 2  # resampling edge
    return (round(widest) + pad * 2,
            round(tallest * (1 + react.get("scale_y", 0.0))) + pad * 2)


def speech_envelope(mp3: Path, fps: int, frames: int, cfg: dict) -> np.ndarray:
    """Per video frame, how loud the mix is, 0..1 — what the sticker reacts to.

    Instant attack, exponential release: the portrait jumps on the syllable and
    settles straight after, which is what squash-and-stretch does on a hit.
    """
    react = cfg.get("react", {})
    out = subprocess.run(
        ["ffmpeg", "-v", "error", "-i", str(mp3), "-af",
         "astats=metadata=1:reset=1,ametadata=print:key=lavfi.astats.Overall.RMS_level:file=-",
         "-f", "null", "-"], capture_output=True, check=True).stdout.decode()
    times, levels = [], []
    for line in out.splitlines():
        if line.startswith("frame:"):
            times.append(float(line.split("pts_time:")[1]))
        elif line.startswith("lavfi.astats"):
            try:
                levels.append(float(line.split("=")[1]))
            except ValueError:
                levels.append(-120.0)
    if not levels:
        return np.zeros(frames, dtype=np.float32)
    # Bin each audio frame into the video frame that covers it, keeping the
    # loudest: a syllable is ~25 ms, a video frame 33 ms.
    stamps = np.asarray(times, dtype=np.float64)
    edges = np.arange(frames + 1, dtype=np.float64) / fps
    bins = np.clip(np.searchsorted(edges, stamps, side="right") - 1, 0, frames - 1)
    per = np.full(frames, -120.0)
    np.maximum.at(per, bins, np.asarray(levels, dtype=np.float64))
    per[bins.max() + 1:] = levels[-1]
    # Loudness varies chapter to chapter, so the window is percentiles of this
    # chapter unless the config pins it.
    lo = react.get("floor_db")
    hi = react.get("ceil_db")
    lo = float(np.percentile(per, react.get("floor_pct", 25))) if lo is None else lo
    hi = float(np.percentile(per, react.get("ceil_pct", 95))) if hi is None else hi
    amp = np.clip((per - lo) / max(hi - lo, 1e-6), 0.0, 1.0)
    decay = float(np.exp(-1.0 / max(react.get("release", 0.14) * fps, 1e-6)))
    out_amp = np.zeros_like(amp)
    hold = 0.0
    for i, v in enumerate(amp):
        hold = max(v, hold * decay)
        out_amp[i] = hold
    return out_amp.astype(np.float32)


def sticker_tile(path: Path, ratio: float, box: tuple[int, int], amount: float,
                 cfg: dict) -> Image.Image:
    """One frame of the portrait, squashed about its centre by `amount`."""
    react = cfg.get("react", {})
    src = sticker_art(path)
    w = max(1, round(w_ratio(src.width, ratio, react, amount, "scale_x", -1)))
    h = max(1, round(w_ratio(src.height, ratio, react, amount, "scale_y", 1)))
    art = src.resize((w, h), Image.LANCZOS)
    out = Image.new("RGBA", box, (0, 0, 0, 0))
    out.alpha_composite(art, ((box[0] - w) // 2, (box[1] - h) // 2))
    return out


def w_ratio(natural: int, ratio: float, react: dict, amount: float,
            key: str, sign: int) -> float:
    return natural * ratio * (1 + sign * react.get(key, 0.0) * amount)


def balanced_lines(words: list[str], font: ImageFont.FreeTypeFont,
                   max_width: float, greedy: list[str]) -> list[str]:
    """Wrap `words` into `len(greedy)` lines of as even a width as possible.

    A two-line caption reads as one block only if the lines are flush, so the
    break moves off the greedy position: among the wrappings that need the same
    number of rows, take the one with the least total line width, which for a
    fixed word set is the one whose lines are closest to equal.
    """
    rows = len(greedy)
    if rows <= 1:
        return greedy
    n = len(words)
    if n < rows:
        return greedy
    gap = font.getlength(" ")
    edges = [0.0]
    for w in words:
        edges.append(edges[-1] + font.getlength(w))

    def width(a: int, b: int) -> float:
        return edges[b] - edges[a] + gap * (b - a - 1)

    inf = float("inf")
    best = [[inf] * (n + 1) for _ in range(rows + 1)]
    cut = [[0] * (n + 1) for _ in range(rows + 1)]
    best[0][0] = 0.0
    for r in range(1, rows + 1):
        for b in range(r, n + 1):
            for a in range(r - 1, b):
                if best[r - 1][a] == inf:
                    continue
                cost = best[r - 1][a] + width(a, b) ** 2
                if cost < best[r][b]:
                    best[r][b] = cost
                    cut[r][b] = a
    if best[rows][n] == inf:
        return greedy
    out: list[list[str]] = []
    b = n
    for r in range(rows, 0, -1):
        a = cut[r][b]
        out.append(words[a:b])
        b = a
    return [" ".join(line) for line in reversed(out)]


def sentences(words: list[str]) -> list[list[str]]:
    """Group words into sentences, so a caption break falls between them."""
    out: list[list[str]] = []
    cur: list[str] = []
    for w in words:
        cur.append(w)
        if w.rstrip(SENTENCE_END) != w:
            out.append(cur)
            cur = []
    if cur:
        out.append(cur)
    return out


def shares_for(dur: float, weights: list[int], min_s: float,
               max_s: float) -> list[float]:
    """Split `dur` between captions in proportion to their text, holding each to
    `min_s`..`max_s` where it can be held.

    The time a clamp takes off one caption has to land on another: clamping a
    long caption to `max_s` and dropping the remainder left the cue's last
    caption ending seconds before the speech did, so the line was still being
    spoken with nothing on screen. What is left over after the clamps always
    goes to the last caption, which owns the cue's final moment; a clamp that
    overshoots into it is paid back by the captions that still have slack.
    """
    n = len(weights)
    wtot = sum(weights) or 1
    shares = [dur * w / wtot for w in weights]
    for i in range(n - 1):  # the last caption absorbs the remainder
        shares[i] = min(max(shares[i], min_s), max_s)
    rest = dur - sum(shares)
    if rest > 0:
        shares[-1] += rest
    elif rest < 0:  # the minimums wanted more than the cue has to give
        slack = [i for i in range(n - 1) if shares[i] > min_s]
        pool = sum(shares[i] - min_s for i in slack)
        if pool > 0:
            for i in slack:
                shares[i] -= -rest * (shares[i] - min_s) / pool
        shares[-1] = max(0.0, shares[-1])
    return shares


def split_cue(text: str, start: float, end: float, font: ImageFont.FreeTypeFont,
              max_width: float, max_lines: int,
              min_s: float, max_s: float, label: str = ""
              ) -> list[tuple[float, float, str]]:
    """Cut one script segment into captions, each wrapped to the frame's width.

    Breaks land on sentence ends: a caption never stops mid-sentence unless the
    sentence alone overflows the rows. Wrapping is by measured pixel width (not
    a character count), and a caption of more than one row is justified, so the
    lines come out flush instead of leaving one word stranded on the last.
    `label` is the speaker's opening quote, closed on the caption's last row.
    """

    def lines_for(words: list[str]) -> list[str]:
        lines, line = [], ""
        for w in words:
            cand = f"{line} {w}" if line else w
            if not line or font.getlength(cand) <= max_width:
                line = cand
            else:
                lines.append(line)
                line = w
        if line:
            lines.append(line)
        return lines

    head = label.split()

    def overflow(w: list[str]) -> bool:
        return bool(w) and len(lines_for(head + w)) > max_lines

    def split_oversized(sent: list[str]) -> list[list[str]]:
        """One sentence too long for the rows: cut it on width alone."""
        out: list[list[str]] = []
        rest = sent
        while overflow(rest):
            take: list[str] = []
            for w in rest:
                if take and overflow(take + [w]):
                    break
                take.append(w)
            if not take:
                break
            out.append(take)
            rest = rest[len(take):]
        if rest:
            out.append(rest)
        return out

    words = text.split()
    if not words:
        return []
    chunks: list[list[str]] = []
    cur: list[str] = []
    for sent in sentences(words):
        for piece in split_oversized(sent):
            cand = cur + piece
            if cur and overflow(cand):
                chunks.append(cur)
                cur = piece
            else:
                cur = cand
    if cur:
        chunks.append(cur)
    dur = max(end - start, 0.001)
    needed = max(1, math.ceil(dur / max_s))
    if needed > len(chunks):  # too long to read in its rows: split further
        sents = sentences(words)
        step = max(1, math.ceil(len(sents) / needed))
        chunks = [[w for s in sents[i:i + step] for w in s]
                  for i in range(0, len(sents), step)]
        # Regrouping whole sentences can overflow the rows; cut those on width.
        chunks = [p for c in chunks for p in (split_oversized(c) if overflow(c) else [c])]
    weights = [sum(len(w) for w in c) for c in chunks]
    wtot = sum(weights)
    shares = shares_for(dur, weights, min_s, max_s)
    out, t = [], start
    for i, c in enumerate(chunks):
        t_end = min(t + shares[i], end)
        rows = c
        if head:  # `Name: "…"` — every caption is its own quotation
            rows = head[:-1] + [head[-1] + c[0]] + c[1:]
            rows[-1] = rows[-1] + '"'
        out.append((t, t_end, "\n".join(balanced_lines(rows, font, max_width,
                                                       lines_for(rows)))))
        t = t_end
    return out


def subtitle_metrics(template: dict, W: int) -> tuple[ImageFont.FreeTypeFont, float]:
    unit = W * template["type"]["unit"] / 100.0
    font = ImageFont.truetype(template["font_files"]["subtitle"],
                              round(unit * template["type"]["subtitle"]))
    return font, 0.94 * W


def ts(seconds: float, comma: bool) -> str:
    seconds = max(0.0, seconds)
    h = int(seconds // 3600)
    m = int((seconds % 3600) // 60)
    s = seconds % 60
    whole = int(s)
    ms = round((s - whole) * 1000)
    if ms == 1000:
        whole, ms = whole + 1, 0
    return f"{h:02d}:{m:02d}:{whole:02d}{',' if comma else '.'}{ms:03d}"


def build_captions(template: dict, chapters: list[dict], starts: list[float],
                   font: ImageFont.FreeTypeFont, max_width: float,
                   fps: int, cfg: dict | None = None) -> list[dict]:
    sub = template.get("subtitle", {})
    max_lines = sub.get("style", {}).get("max_lines", 2)
    min_s = sub.get("min_s", 1.0)
    max_s = sub.get("max_s", 7.0)
    caps = []
    for ch, off in zip(chapters, starts):
        # The cue sheet is written on the planned clock and can outrun the mp3 it
        # describes by a second or so; a caption must not claim time the video
        # does not have, or the last line of a chapter is cut off mid-word.
        limit = off + ch["dur"]
        for cue in ch["cues"]:
            text = caption_text(cue["text"])
            if not text or off + cue["start"] >= limit - 0.05:
                continue
            sticker = sticker_for(cue["speaker"], cfg or {})
            for (a, b, line) in split_cue(text, min(off + cue["start"], limit),
                                          min(off + cue["end"], limit),
                                          font, max_width,
                                          max_lines, min_s, max_s,
                                          speaker_label(cue["speaker"])):
                caps.append({"start": a, "end": b, "text": line,
                             "speaker": cue["speaker"],
                             "sticker": sticker.as_posix() if sticker else None})
    caps.sort(key=lambda c: c["start"])
    # The video can only change a caption on a frame boundary, so the sidecars
    # carry the frame times the burn-in actually uses.
    for c in caps:
        c["start"] = round(c["start"] * fps) / fps
        c["end"] = max(round(c["end"] * fps), round(c["start"] * fps) + 1) / fps
    for prev, nxt in zip(caps, caps[1:]):
        prev["end"] = min(prev["end"], nxt["start"])
    return caps


def write_subtitles(caps: list[dict], srt_path: Path, vtt_path: Path) -> None:
    srt = []
    for i, c in enumerate(caps, 1):
        srt.append(f"{i}\n{ts(c['start'], True)} --> {ts(c['end'], True)}\n{c['text']}\n")
    srt_path.write_text("\n".join(srt), encoding="utf-8")
    vtt = ["WEBVTT", ""]
    for c in caps:
        vtt.append(f"{ts(c['start'], False)} --> {ts(c['end'], False)}\n{c['text']}\n")
    vtt_path.write_text("\n".join(vtt), encoding="utf-8")


# --------------------------------------------------------------------------- #
# timed strips: a concat of still images, encoded for overlay
# --------------------------------------------------------------------------- #


def strip_video(entries: list[tuple[Path, float]], parts: Path, tag: str, fps: int) -> Path:
    list_path = parts / f"{tag}.txt"
    with list_path.open("w", encoding="utf-8") as f:
        f.write("ffconcat version 1.0\n")
        # Absolute paths: the demuxer resolves a relative one against the list
        # file's own directory, which prefixes it a second time.
        for path, dur in entries:
            f.write(f"file '{path.resolve().as_posix()}'\n")
            f.write(f"duration {max(dur, 0.001):.3f}\n")
        if entries:  # the concat demuxer holds the last frame this way
            f.write(f"file '{entries[-1][0].resolve().as_posix()}'\n")
    out = parts / f"{tag}.mp4"
    subprocess.run(["ffmpeg", "-y", "-hide_banner", "-loglevel", "error", "-nostdin",
                    "-f", "concat", "-safe", "0", "-i", str(list_path),
                    "-vf", f"fps={fps},format=yuv420p",
                    "-c:v", "libx264", "-preset", "veryfast", "-crf", "18", str(out)],
                   check=True)
    return out


def png_bytes(im: Image.Image) -> bytes:
    buf = io.BytesIO()
    im.save(buf, format="PNG", compress_level=1)
    return buf.getvalue()


def alpha_strip_video(frames: list[bytes], parts: Path, tag: str, fps: int) -> Path:
    """Encode one RGBA frame per element of `frames` — no concat demuxer.

    The concat demuxer quantises a still's duration to the input timebase, so a
    run of 0.033s fade frames drifted the captions off their own timings. Piping
    an explicit frame list puts every frame on exactly 1/fps.
    """
    out = parts / f"{tag}.mov"
    p = subprocess.Popen(["ffmpeg", "-y", "-hide_banner", "-loglevel", "error", "-nostdin",
                          "-f", "image2pipe", "-vcodec", "png", "-framerate", str(fps),
                          "-i", "pipe:0", "-c:v", "qtrle", str(out)], stdin=subprocess.PIPE)
    try:
        for data in frames:
            p.stdin.write(data)
        p.stdin.close()
    except BrokenPipeError:  # ffmpeg gave up; its exit status is the real news
        pass
    if p.wait() != 0:
        sys.exit(f"ffmpeg failed to encode {tag}.mov")
    return out


def caption_strip(template: dict, caps: list[dict], boxes: dict, total: float,
                  W: int, parts: Path, fps: int, fade_s: float,
                  sticker: tuple[dict, tuple[int, int], float, np.ndarray] | None = None
                  ) -> tuple[Path, Path | None, tuple[int, int], tuple[int, int]]:
    """Transparent caption tiles as an overlay strip, fading in and out.

    A tile is glyphs on alpha, never a colour field, so the paper underneath
    shows through. A caption fades up over its first frames and down over its
    last, and is on screen nowhere else: fading the change itself would show the
    next line before its own subtitle time, which read as a line appearing,
    vanishing in the gap between speakers, and reappearing. The ramps are baked
    into blended tiles rather than left to a filter, so the composite stays one
    overlay however many captions there are.

    `sticker` is (config, box, ratio, envelope): the portrait is drawn in the
    same pass, frame for frame with the captions, so it reacts while its own
    speaker talks and is still when they do not. Returns the caption strip, the
    sticker strip (None without one), and where each one lands: the caption
    zone's top-left and the sticker's offset in it.
    """
    zone = boxes["subtitle"]
    zx, zy = round(zone["left"]), round(zone["top"])
    # even dimensions: the encoders refuse an odd width or height
    zw = round(zone["width"]) & ~1
    zh = round(zone["height"]) & ~1
    fs = W * template["type"]["unit"] / 100.0 * template["type"]["subtitle"]
    font, _ = subtitle_metrics(template, W)
    ink = parse_color(template["palette"]["subtitle"])
    lh = fs * 1.3
    Image.new("RGBA", (zw, zh), (0, 0, 0, 0)).save(parts / "blank.png")

    def render(text: str) -> Image.Image:
        im = Image.new("RGBA", (zw, zh), (0, 0, 0, 0))
        d = ImageDraw.Draw(im)
        lines = text.split("\n")
        top = (zh - lh * len(lines)) / 2
        for k, l in enumerate(lines):
            d.text(((zw - d.textlength(l, font=font)) / 2, top + k * lh), l,
                   font=font, fill=ink)
        return im

    cfg = sticker[0] if sticker else {}
    box = sticker[1] if sticker else (0, 0)
    ratio = sticker[2] if sticker else 1.0
    env = sticker[3] if sticker else None

    tiles, encoded = [], []
    for i, c in enumerate(caps):
        im = render(c["text"])
        im.save(parts / f"cap-{i:05d}.png")
        tiles.append(np.asarray(im, dtype=np.float32))
        encoded.append(png_bytes(im))
    empty = np.zeros((zh, zw, 4), dtype=np.float32)
    blank = png_bytes(Image.fromarray(empty.astype(np.uint8), "RGBA"))

    def blend(a: np.ndarray, b: np.ndarray, u: float) -> bytes:
        """One ramp frame: `a` at full strength fading to `b` at `u`."""
        alpha = a[..., 3] * (1.0 - u) + b[..., 3] * u
        pre = a[..., :3] * (a[..., 3:4] / 255.0) * (1.0 - u) + b[..., :3] * (b[..., 3:4] / 255.0) * u
        rgb = np.where(alpha[..., None] > 0, pre * 255.0 / np.maximum(alpha[..., None], 1e-6), 0.0)
        im = np.dstack([np.clip(rgb, 0, 255), np.clip(alpha, 0, 255)]).astype(np.uint8)
        return png_bytes(Image.fromarray(im, "RGBA"))

    # Caption times are already on the frame grid, so a caption's span is a whole
    # number of frames: the ramps are its first and last few.
    fade = max(1, round(fade_s * fps))
    frames: list[bytes] = []
    stickers: list[bytes] = []
    s_blank = png_bytes(Image.new("RGBA", box, (0, 0, 0, 0))) if sticker else None
    s_cache: dict[tuple, bytes] = {}
    levels = 16

    def s_tile(path: str, amount: float, alpha: float) -> bytes:
        """The portrait for one frame, squashed by the speech envelope, and
        faded in step with the caption. Cached: the squash is quantised, so a
        chapter reuses a few hundred tiles."""
        amount = round(amount * levels) / levels
        key = (path, amount, round(alpha * 64))
        if key not in s_cache:
            tile = sticker_tile(Path(path), ratio, box, amount, cfg)
            arr = np.asarray(tile, dtype=np.float32)
            arr[..., 3] *= alpha
            s_cache[key] = png_bytes(Image.fromarray(
                np.clip(arr, 0, 255).astype(np.uint8), "RGBA"))
        return s_cache[key]

    def ramps(span: int) -> tuple[int, int, list[float], list[float]]:
        n_in = min(fade, max(1, span // 3))
        n_out = min(fade, max(0, span - n_in - 1))
        # The ramps stop short of the empty tile: a frame at zero alpha between
        # two captions is the one-frame flash this replaces. `blend(a, b, u)`
        # runs a -> b, so the way up starts from the empty tile.
        return (n_in, n_out,
                [k / (n_in + 1) for k in range(1, n_in + 1)],
                [k / (n_out + 1) for k in range(1, n_out + 1)])

    # The portrait lives for a *run* of consecutive captions from one speaker,
    # not one caption: it fades in on the run's first frame and out on its last,
    # and is solid across every dialogue change inside. Fading each caption made
    # a character who speaks three lines in a row blink twice per line.
    # A run breaks when the speaker changes or a captionless gap opens, so a
    # narrator line between two of theirs still clears the portrait.
    runs: list[list] = []  # [path, first_frame, last_frame]
    for i, c in enumerate(caps):
        path = c.get("sticker")
        if not path:
            continue
        start, end = round(c["start"] * fps), round(c["end"] * fps)
        if runs and runs[-1][0] == path and start <= runs[-1][2]:
            runs[-1][2] = max(runs[-1][2], end)
        else:
            runs.append([path, start, end])

    # How loudly each speaker is talking, frame by frame. The envelope comes off
    # the mixed chapter, so it is gated to the frames of that speaker's own cues:
    # otherwise the portrait squashed to the narrator, to other characters and to
    # the music bed, and it moved while its own character was silent.
    talk = np.zeros(round(total * fps) + 1, dtype=np.float32)
    for i, c in enumerate(caps):
        if not c.get("sticker") or env is None:
            continue
        a, b = round(c["start"] * fps), round(c["end"] * fps)
        lo, hi = min(a, len(talk) - 1), min(b, len(talk))
        talk[lo:hi] = np.maximum(talk[lo:hi], env[lo:hi])
    at = 0
    for i, c in enumerate(caps):
        start, end = round(c["start"] * fps), round(c["end"] * fps)
        gap = max(0, start - at)
        frames.extend([blank] * gap)
        stickers.extend([s_blank] * gap)
        n_in, n_out, ups, downs = ramps(end - start)
        hold = max(0, end - start - n_in - n_out)
        frames.extend(blend(empty, tiles[i], u) for u in ups)
        frames.extend([encoded[i]] * hold)
        frames.extend(blend(tiles[i], empty, u) for u in downs)
        stickers.extend([s_blank] * (n_in + hold + n_out))
        at = end
    # The portrait, laid over the run the captions just described.
    for path, a, b in runs:
        n_in, n_out, ups, downs = ramps(b - a)
        hold = max(0, b - a - n_in - n_out)
        for k, u in enumerate(ups):
            stickers[a + k] = s_tile(path, float(talk[a + k]), u)
        for k in range(hold):
            stickers[a + n_in + k] = s_tile(path, float(talk[a + n_in + k]), 1.0)
        for k, u in enumerate(downs):
            # `downs` ascends because blend() runs a -> b; as a direct alpha
            # multiplier it has to be inverted, or the badge brightens on its
            # way out instead of fading.
            f = a + n_in + hold + k
            stickers[f] = s_tile(path, float(talk[f]), 1.0 - u)
    tail = max(0, round(total * fps) - at)
    frames.extend([blank] * tail)
    stickers.extend([s_blank] * tail)
    subs = alpha_strip_video(frames, parts, "subs", fps)
    stick = alpha_strip_video(stickers, parts, "stickers", fps) if sticker else None
    # The portrait sits on the caption's optical centre: a line's baseline less
    # half its ascender. Anchored to a single line's box so it holds still
    # whatever row count the caption wraps to.
    ascent = font.getmetrics()[0]
    # The badge sits above the captions, centred on the frame, at a fixed spot.
    # It used to sit in a gutter at the left of the line: the text is re-wrapped
    # and re-centred for every caption, so a portrait beside it has to move with
    # it, and a badge that slides between two fixed positions reads as leaving
    # the line it belongs to. Fixed above the zone, it is simply there while its
    # character talks.
    fx, fy = (cfg.get("at") or [0.5, 740 / 1080])[:2]
    at_stick = (round(W * fx - box[0] / 2),
                round(template["canvas"]["size"][1] * fy - box[1] / 2)) if sticker else (zx, zy)
    return subs, stick, (zx, zy), at_stick


# --------------------------------------------------------------------------- #
# inputs
# --------------------------------------------------------------------------- #


def find_chapter(workspace: Path, n: int) -> tuple[Path, Path]:
    hits = sorted((workspace / "output").glob(f"Ch.{n} - *.mp3"))
    if not hits:
        sys.exit(f"no published mp3 for chapter {n} under {workspace / 'output'}")
    mp3 = hits[0]
    cues = mp3.parent / (mp3.stem + ".cues.json")
    if not cues.exists():
        sys.exit(f"chapter {n} has no cue sidecar ({cues.name}) — merge it first")
    return mp3, cues


def probe_duration(path: Path) -> float:
    out = subprocess.check_output(
        ["ffprobe", "-v", "error", "-show_entries", "format=duration",
         "-of", "csv=p=0", str(path)], text=True)
    return float(out.strip())


# --------------------------------------------------------------------------- #
# render
# --------------------------------------------------------------------------- #


def render(workspace: Path, manifest: dict, template: dict, outdir: Path, name: str,
           gap_s: float, preview: float | None, no_subs: bool, dry_run: bool,
           keep_parts: bool = False, reuse_parts: bool = False) -> Path:
    W, H = template["canvas"]["size"]
    fps = template["canvas"].get("fps", 30)
    # The strips are pre-rendered, so the final preset is quality paid in
    enc = template.get("encode", {})
    preset, crf = enc.get("preset", "veryfast"), int(enc.get("crf", 18))
    acts = manifest["acts"]

    chapters = []
    for act in acts:
        for n in act["chapters"]:
            mp3, cues_path = find_chapter(workspace, n)
            cues = json.loads(cues_path.read_text(encoding="utf-8"))["cues"]
            chapters.append({"n": n, "act": act["act"], "mp3": mp3, "cues": cues,
                             "dur": probe_duration(mp3)})
    starts, t = [], 0.0
    for ch in chapters:
        starts.append(t)
        t += ch["dur"] + gap_s
    total = starts[-1] + chapters[-1]["dur"]
    if preview:
        total = min(total, preview)

    # One act's window runs from its first chapter to the next act's first chapter.
    windows = []
    for i, act in enumerate(acts):
        start = next(s for c, s in zip(chapters, starts) if c["act"] == act["act"])
        boundaries = [s for c, s in zip(chapters, starts) if c["act"] != act["act"] and s > start]
        end = min(boundaries) if boundaries else total
        windows.append((act, start, min(end, total)))
    act_durs = [end - start for _, start, end in windows]

    outdir.mkdir(parents=True, exist_ok=True)
    parts = outdir / f"{name}.parts"
    # The strips are the expensive half of a render, so a complete set from a
    # previous run can be kept whole — but a partial one is worse than none.
    needed = ["static.png", "titles.mp4", "thumb.mov"]
    if not no_subs:
        needed += ["subs.mov", "stickers.mov"]
    reused = reuse_parts and parts.exists() and all((parts / f).exists() for f in needed)
    if reused:
        print(f"{name}: reusing the strips in {parts.name}/ (--reuse-parts)")
    else:
        if parts.exists():
            shutil.rmtree(parts)
        parts.mkdir(parents=True)
    srt_path, vtt_path = outdir / f"{name}.srt", outdir / f"{name}.vtt"
    out_mp4 = outdir / f"{name}.mp4"

    static = build_static(template, W, H)
    static_png = parts / "static.png"
    static.save(static_png)
    boxes = zone_boxes(template, W, H)
    sub_font, sub_width = subtitle_metrics(template, W)
    s_cfg = sticker_config(template)
    speakers = {c["speaker"] for ch in chapters for c in ch["cues"]}
    s_box = sticker_box(template, s_cfg, speakers, W) if s_cfg else (0, 0)
    caps = build_captions(template, chapters, starts, sub_font, sub_width, fps,
                          s_cfg)
    if not no_subs:
        write_subtitles(caps, srt_path, vtt_path)
    print(f"{name}: {len(acts)} acts, {len(chapters)} chapters, {ts(total, False)} total, "
          f"{len(caps)} captions")

    # ---- inputs: 0 static, then the title strip, the caption strip, the thumb ----
    cmd = ["ffmpeg", "-y", "-hide_banner", "-loglevel", "error", "-nostdin",
           "-loop", "1", "-framerate", str(fps), "-i", str(static_png)]
    idx = 1
    title_entries = [(act_title_plate(template, boxes, act, static, W, parts, i), end - start)
                     for i, (act, start, end) in enumerate(windows)]
    title_strip = strip_video(title_entries, parts, "titles", fps)
    cmd += ["-i", str(title_strip)]
    title_idx = idx
    idx += 1
    subs_idx = stick_idx = zx = zy = stick_at = None
    if not no_subs and caps:
        fade = min(0.5, max(0.1, template.get("subtitle", {}).get("fade_s", 0.3)))
        env = None
        if s_cfg and any(c["sticker"] for c in caps):
            env = np.zeros(round(total * fps), dtype=np.float32)
            for ch, off in zip(chapters, starts):
                per = speech_envelope(ch["mp3"], fps, round(ch["dur"] * fps), s_cfg)
                a = round(off * fps)
                b = min(len(env), a + len(per))
                env[a:b] = per[: b - a]
        sticker = (s_cfg, s_box, sticker_ratio(template), env) if env is not None else None
        subs_strip, sticker_strip, (zx, zy), stick_at = caption_strip(
            template, caps, boxes, total, W, parts, fps, fade, sticker)
        cmd += ["-i", str(subs_strip)]
        subs_idx = idx
        idx += 1
        if sticker_strip:
            cmd += ["-i", str(sticker_strip)]
            stick_idx = idx
            idx += 1
    thumb, spin = thumb_image(template, H)
    thumb_idx = None
    if thumb:
        thumb_mov = thumb_movie(template, parts, fps, spin, thumb)
        # a real (looped) clip, not `-loop 1` on a still: that stalls after a
        # few hundred seconds and freezes the thumb mid-video
        cmd += ["-stream_loop", "-1", "-i", str(thumb_mov)]
        thumb_idx = idx
        idx += 1
    audio_base = idx
    for ch in chapters:
        cmd += ["-i", str(ch["mp3"])]

    # ---- the bar: one base segment per act, the active one on top ------------ #
    geom = timeline_geometry(template, act_durs, W, H)
    base_c = parse_color(template["palette"]["timeline_segment"])
    on_c = parse_color(template["palette"]["timeline_segment_active"])
    scale = template["timeline"].get("segment", {}).get("active_y_scale", 1.4)
    cy, th = geom["cy"], geom["track_h"]
    boxes_str = [drawbox(geom["left"], cy - th / 2, geom["width"], th, base_c, alpha=0.35)]
    for x, w in geom["segs"]:
        boxes_str.append(drawbox(x, cy - th / 2, w, th, base_c))
    for (x, w), (_, start, end) in zip(geom["segs"], windows):
        boxes_str.append(drawbox(x, cy - th * scale / 2, w, th * scale, on_c,
                                 enable=f"between(t,{start:.3f},{end:.3f})"))
    video = [f"[0:v]scale={W}:{H},setsar=1[bg]",
             f"[bg]{','.join(boxes_str)}[v0]"]
    cur, n = "v0", 0

    def chain(idx_label: str, overlay: str) -> None:
        nonlocal cur, n
        n += 1
        video.append(f"[{cur}]{overlay}[v{n}]")
        cur = f"v{n}"

    if thumb_idx is not None:
        video.append(f"[{thumb_idx}:v]null[th]")
        x = f"{geom['left']:.2f}+(t/{total:.3f})*{geom['width']:.2f}-{thumb.width / 2:.2f}"
        y = f"{cy - thumb.width / 2:.2f}"
        chain(str(thumb_idx), f"[th]overlay=x='{x}':y={y}:format=auto:shortest=0")
    chain(str(title_idx), f"[{title_idx}:v]overlay=x={round(boxes['act_title']['left'])}"
                          f":y={round(boxes['act_title']['top'])}:format=auto:shortest=0")
    if subs_idx is not None:
        # format=rgba: the tile's alpha must survive into the blend, or the
        # transparent ground is composited as black.
        video.append(f"[{subs_idx}:v]format=rgba[subs]")
        chain(str(subs_idx),
              f"[subs]overlay=x={zx}:y={zy}:format=auto:shortest=0")
    if stick_idx is not None:
        video.append(f"[{stick_idx}:v]format=rgba[stickin]")
        chain(str(stick_idx),
              f"[stickin]overlay=x={stick_at[0]}:y={stick_at[1]}:format=auto:shortest=0")
    video.append(f"[{cur}]null[v]")

    labels, audio = [], []
    for i in range(len(chapters)):
        lab = f"a{i}"
        labels.append(f"[{lab}]")
        audio.append(f"[{audio_base + i}:a]apad=pad_dur={gap_s:.3f}[{lab}]")
    audio.append(f"{''.join(labels)}concat=n={len(chapters)}:v=0:a=1[aud]")
    fc = ";".join(video + audio)

    cmd += ["-filter_complex", fc, "-map", "[v]", "-map", "[aud]",
            "-c:v", "libx264", "-preset", preset, "-crf", str(crf),
            "-pix_fmt", "yuv420p", "-r", str(fps),
            "-c:a", "aac", "-b:a", "256k",
            "-t", f"{total:.3f}", "-movflags", "+faststart", str(out_mp4)]

    print(" ".join(cmd))
    if dry_run:
        return out_mp4
    subprocess.run(cmd, check=True)
    # Strips the run did not build are not the run's to delete.
    if not keep_parts and not reused:
        for p in parts.iterdir():
            p.unlink()
        parts.rmdir()
    print(f"wrote {out_mp4}")
    return out_mp4


def build_static(template: dict, W: int, H: int) -> Image.Image:
    return paper(template, W, H)


def normalize(manifest: dict) -> dict:
    """A single act or a list of them; the render always sees `acts`."""
    if "acts" in manifest:
        return manifest
    return {"acts": [{"act": manifest.get("act", 1), "title": manifest["title"],
                      "chapters": manifest["chapters"]}]}


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--workspace", type=Path,
                    default=ROOT / "workspaces" / "beyond-myriads",
                    help="book workspace holding output/ (default: beyond-myriads)")
    ap.add_argument("--acts", type=Path, required=True,
                    help="acts manifest: {acts: [{act, title, chapters}]}")
    ap.add_argument("--template", type=Path, default=ROOT / "tools" / "video-template.json")
    ap.add_argument("--outdir", type=Path, default=ROOT / "renders")
    ap.add_argument("--name", default="")
    ap.add_argument("--chapter-gap", type=float, default=0.8,
                    help="seconds of silence between chapters (default 0.8)")
    ap.add_argument("--preview", type=float, default=None,
                    help="render only the first N seconds (a timing check)")
    ap.add_argument("--no-subs", action="store_true",
                    help="skip burning; still write the .srt/.vtt sidecars")
    ap.add_argument("--dry-run", action="store_true")
    ap.add_argument("--keep-parts", action="store_true",
                    help="keep the intermediate strips for inspection")
    ap.add_argument("--reuse-parts", action="store_true",
                    help="render from an existing complete acts-NN-NN.parts/ instead of rebuilding")
    args = ap.parse_args()

    workspace = args.workspace if args.workspace.is_absolute() else ROOT / args.workspace
    manifest = normalize(json.loads(Path(args.acts).read_text(encoding="utf-8")))
    template = load_template(args.template)
    numbers = [a["act"] for a in manifest["acts"]]
    name = args.name or f"acts-{min(numbers):02d}-{max(numbers):02d}"
    render(workspace, manifest, template, args.outdir, name,
           args.chapter_gap, args.preview, args.no_subs, args.dry_run, args.keep_parts,
           args.reuse_parts)


if __name__ == "__main__":
    main()
