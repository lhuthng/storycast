#!/usr/bin/env python3
"""Add one sound — a **place** (effect bed) or a **moment** (inject one-shot) —
to a pack under `assets/_extends/`.

    tools/add-sound.py [--apply] [--tags a,b] [--mode M] [--hold S] [--gain F]
                       [--once] [--alias a,b] [--alias-to tag]
                       <pack> place|moment <key> <clip> [clip...]

This is the general form of tools/ingest-clips.tmp.py's registry half, which
remained a batch one-shot. Tracks (the third kind) already have their end-to-end
path: `tools/gen-sound.sh one --level track` -> `tools/add-music.sh`, which
registers as it installs. A place or a moment had the file half (`gen-sound.sh
one --level place|moment` lands a normalized file) and no registry half; this
closes it, and it is safe to point at a clip `gen-sound.sh` already normalized —
the spec check passes through anything already at its layer's target.

    tools/add-sound.py craft moment paper-powder take1.wav take2.wav \
        --tags paper,packet,wrap,powder,measure --alias folding-paper,wrapping
    tools/add-sound.py craft place courtyard-dawn take1.wav \
        --tags courtyard,dawn,stone --alias apothecary-courtyard

What it does, in order:
  1. **spec check** — measures the clip (codec, sample rate, channels,
     integrated LUFS). A clip that is not 48k mono mp3 at its layer's target
     goes through `tools/normalize-audio.sh` (-26 LUFS for a place, -20 for a
     moment, the same rungs the pool's docs pin) into gitignored staging; a
     clip already at spec passes untouched. A recording the member already
     holds (same content hash) is refused — two takes of one recording defeat
     the picker's second roll.
  2. **place the file(s)** — `effects/<key>-<N>.mp3` / `injects/<key>-<N>.mp3`,
     N continuing the family's existing numbering (a family that starts at -2
     reads as if a take were missing).
  3. **register the take(s)** — a new key inserts a member in the file's own
     house style; an existing key gains takes in its `files` (a moment's
     `dur_s` only grows: it clamps holds and tails). Each pool is edited in
     **its own spelling**, detected from the file itself: a pool that
     round-trips under the Rust writer (indent 2, or common's indent 1) is
     rewritten whole under a byte-exact guard; a hand-formatted pool is
     touched only at the exact byte spans, so nobody's formatting reflows.
     `_note` is never touched either way.
  4. **aliases** — `--alias` words go to `tag-aliases.json`: a moment's words
     map to the canonical key under `sound`; a place's words map to a bed tag
     under `effect` (the table maps place words to *tags*, which is what the
     scene map's rules match), defaulting to the sound's first tag. Refused if
     the word is claimed anywhere.
  5. **prove it** — runs `tools/inspect-pool.py` on the pack, so the row is
     seen on disk before anything is released.

Dry run by default; --apply writes. After applying, the live `assets/` tree is
stale until the next resolve, and the *release* is what a box sees: re-cut it
and the next provision diffs the box's receipt and sends only the delta — the
receipt itself is never edited by hand.

    ./rust/target/release/bm-inductor asset resolve --dry-run
    ./rust/target/release/bm-inductor profile manifest <pack> --piece pack --dep
"""

import json
import hashlib
import os
import pathlib
import re
import shutil
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
# BM_ASSETS moves the whole tree for a test run; everything else is fixed,
# because the resolved tree's shape is what the tool's edits must survive.
ASSETS = pathlib.Path(os.environ.get("BM_ASSETS", ROOT / "assets"))
EXTENDS = ASSETS / "_extends"
# Staging lives outside the pack: anything under `assets/` ships in the release,
# and a staging folder there once was 54 MB, 31% of a manifest.
STAGING = ROOT / "refs" / "temp" / "clips" / "add-sound"

# The layers' loudness rungs, from SOUND.md / README: a bed sits far under the
# voice, a foreground inject lands with the peaks. The same numbers
# normalize-audio.sh takes as I_TARGET.
LUFS_TARGET = {"place": "-26", "moment": "-20"}
LUFS_TOLERANCE = 0.7  # measured within this of target -> already normalized
SPEC_RATE = 48000
POOL_FILE = {"place": "effect-pool.json", "moment": "inject-pool.json"}
SUBDIR = {"place": "effects", "moment": "injects"}


def die(msg):
    print(f"REFUSED: {msg}")
    sys.exit(1)


def load(p):
    return json.loads(p.read_text())


def sha(p):
    return hashlib.sha256(p.read_bytes()).hexdigest()[:12]


def ffprobe(p, entries):
    out = subprocess.run(
        ["ffprobe", "-v", "error", "-show_entries", entries, "-of", "json", str(p)],
        capture_output=True, text=True, check=True).stdout
    return json.loads(out)


def dur_s(p):
    return round(float(ffprobe(p, "format=duration")["format"]["duration"]), 1)


def spec(p):
    st = ffprobe(p, "stream=sample_rate,channels")["streams"][0]
    return int(st["sample_rate"]), int(st["channels"])


def lufs(p):
    """Integrated loudness — the measurement normalize-audio.sh applies."""
    out = subprocess.run(
        ["ffmpeg", "-nostdin", "-hide_banner", "-nostats", "-i", str(p),
         "-af", "loudnorm=print_format=json", "-f", "null", "-"],
        capture_output=True, text=True, check=True).stderr
    m = re.search(r"\{[^{}]*\"input_i\"[^{}]*\}", out, re.S)
    return float(json.loads(m.group(0))["input_i"]) if m else None


def at_spec(p, level):
    """True when the clip is already a pool clip: 48k mono mp3 at the layer's
    rung. Nothing is re-normalized in that case — double loudnorm drifts, and
    a clip gen-sound.sh landed is already on spec."""
    if p.suffix.lower() != ".mp3":
        return False
    rate, channels = spec(p)
    if rate != SPEC_RATE or channels != 1:
        return False
    measured = lufs(p)
    return measured is not None and abs(measured - float(LUFS_TARGET[level])) <= LUFS_TOLERANCE


def bring_to_spec(src, level):
    """One clip at the layer's rung: pass-through, or normalize-audio.sh into
    staging. Returns the ready file."""
    if at_spec(src, level):
        print(f"      on spec ({spec(src)[0]} Hz/{spec(src)[1]}ch, "
              f"{lufs(src):.1f} LUFS vs {LUFS_TARGET[level]}) — passed through")
        return src
    sdir = STAGING / "src"
    odir = STAGING / "out"
    sdir.mkdir(parents=True, exist_ok=True)
    odir.mkdir(parents=True, exist_ok=True)
    for stale in odir.iterdir():
        stale.unlink()
    staged = sdir / src.name
    if staged.exists():
        staged.unlink()
    shutil.copy2(src, staged)
    subprocess.run(
        ["bash", str(ROOT / "tools" / "normalize-audio.sh"), str(sdir), str(odir)],
        check=True, capture_output=True, text=True,
        env={**os.environ, "I_TARGET": LUFS_TARGET[level]})
    out = odir / (src.stem + ".mp3")
    if not out.exists():
        die(f"normalize-audio.sh produced nothing for {src.name} "
            f"(it may have trimmed to digital silence)")
    return out


def pool_keys(doc):
    return [k for k in doc if k != "_note"]


def load_text(text):
    return json.loads(text)


def spelling(text, doc):
    """How this pool is written, as (kind, indent). `machine` means the Rust
    writer reproduces it byte for byte at that indent, so a guarded whole
    rewrite is safe; `hand` means someone formatted it themselves, and only
    the exact byte spans may be touched."""
    for n in (2, 1):
        if json.dumps(doc, indent=n, ensure_ascii=False) + "\n" == text:
            return ("machine", n)
    m = re.search(r'^(\s+)"(?!_note)[A-Za-z0-9_-]+": \{', text, re.M)
    return ("hand", len(m.group(1)) if m else 2)


def render_member(key, entry, pool_kind, kind, ind):
    """One pool member in the file's own house style. A machine pool gets the
    writer's one-element-per-line arrays and field order; a hand pool gets its
    inline-arrays convention, wrapped past 96 columns (craft's rule)."""
    pad = " " * ind
    pad2 = pad * 2
    pad3 = pad * 3
    lines = [f'{pad}"{key}": {{']
    if kind == "machine":
        def arr(name, items):
            body = ",\n".join(f'{pad3}"{x}"' for x in items)
            return f'{pad2}"{name}": [\n{body}\n{pad2}]'
        lines.append(arr("tags", entry["tags"]) + ",")
        lines.append(arr("files", entry["files"]) + ",")
        lines.append(f'{pad2}"mode": "{entry["mode"]}",')
        if entry.get("hold") is not None:
            lines.append(f'{pad2}"hold": {entry["hold"]},')
        lines.append(f'{pad2}"level": {entry["level"]},')
        lines.append(f'{pad2}"looped": {str(entry["looped"]).lower()},')
        lines.append(f'{pad2}"dur_s": {entry["dur_s"]}')
    else:
        def arr(name, items, tail):
            inline = f'{pad2}"{name}": [' + ", ".join(f'"{x}"' for x in items) + "]" + tail
            if len(inline) <= 96:
                return inline
            body = ",\n".join(f'{pad3}"{x}"' for x in items)
            return f'{pad2}"{name}": [\n{body}\n{pad2}]{tail}'
        lines.append(arr("tags", entry["tags"], ","))
        lines.append(arr("files", entry["files"], ","))
        if pool_kind == "place":
            # a bed writes `looped` only when it is not the default
            if not entry.get("looped", True):
                lines.append(f'{pad2}"looped": false')
        else:
            lines.append(f'{pad2}"mode": "{entry["mode"]}",')
            lines.append(f'{pad2}"level": {entry["level"]},')
            lines.append(f'{pad2}"looped": {str(entry["looped"]).lower()},')
            if entry.get("hold") is not None:
                lines.append(f'{pad2}"hold": {entry["hold"]},')
            lines.append(f'{pad2}"dur_s": {entry["dur_s"]}')
    # A default-looping bed writes no `looped` line, so the member's last
    # field line may carry a comma JSON cannot hold.
    body = "\n".join(lines)
    return (body[:-1] if body.endswith(",") else body) + f"\n{pad}}}"


def member_span(text, key, ind):
    """The exact (start, end) byte span of a member, found from its own key's
    indent and closed at the first line that is just that indent plus `}`.
    Pool members are flat — arrays close at a deeper indent — so the first
    such line is the member's own brace."""
    pad = " " * ind
    start = text.find(f'\n{pad}"{key}": {{')
    if start < 0:
        return None
    close = text.find(f'\n{pad}}}', start + 1)
    if close < 0:
        return None
    return start, close + 1 + len(pad) + 1


def insert_member(text, key, block, ind):
    """Insert a top-level member, keeping every existing byte."""
    pad = " " * ind
    keys = [m.group(1) for m in re.finditer(rf'^{pad}"([^"]+)": \{{', text, re.M)]
    if key in keys:
        die(f'"{key}" already exists in this pool')
    later = [k for k in keys if k > key]
    if later:
        m = re.search(rf'\n{pad}"{re.escape(later[0])}": \{{', text)
        return text[:m.start() + 1] + block + ",\n" + text[m.start() + 1:]
    assert text.endswith("}\n"), "unexpected pool tail"
    return text[:-2].rstrip("\n") + ",\n" + block + "\n}\n"


def extend_member_files(text, key, ind, subdir, new_files):
    """Extend an existing member's `files`, in place at the byte span."""
    span = member_span(text, key, ind)
    if not span:
        die(f'"{key}" not found in the pool')
    member = text[span[0]:span[1]]
    allf = re.findall(rf'"({subdir}/[^"]+)"', member)
    if any(f in allf for f in new_files):
        die(f"{new_files[0]} is already a take of {key}")
    merged = allf + new_files
    inline = f'"files": [' + ", ".join(f'"{f}"' for f in merged) + "]"
    pad2 = " " * (ind * 2)
    if len(pad2 + inline) <= 96 or ind == 1 and len(pad2 + inline) <= 96:
        block = pad2 + inline
    else:
        block = (f'{pad2}"files": [\n'
                 + ",\n".join(f'{" " * (ind * 3)}"{f}"' for f in merged)
                 + f'\n{pad2}]')
    new_member = re.sub(r'"files": \[.*?\]', block, member, count=1, flags=re.S)
    if new_member == member:
        die(f"could not extend {key}'s files")
    return text[:span[0]] + new_member + text[span[1]:]


def extend_member_dur(text, key, ind, value):
    """Grow one member's `dur_s`, in place. It only ever grows: it clamps
    holds and tails, and a shorter take must not shorten the family's clamp."""
    span = member_span(text, key, ind)
    if not span:
        die(f'"{key}" not found in the pool')
    member = text[span[0]:span[1]]
    new_member, n = re.subn(r'"dur_s":\s*[0-9.]+', f'"dur_s": {value}', member)
    if n != 1:
        die(f"could not update {key}'s dur_s")
    return text[:span[0]] + new_member + text[span[1]:]


def claimed_names():
    """Every alias word claimed anywhere, plus every canonical sound key."""
    names = set()
    for pack_dir in sorted(EXTENDS.iterdir()):
        if not pack_dir.is_dir():
            continue
        ap = pack_dir / "tag-aliases.json"
        if ap.exists():
            d = load(ap)
            for kind in ("sound", "effect", "music"):
                names |= set((d.get(kind) or {}).keys())
        for pool in POOL_FILE.values():
            p = pack_dir / pool
            if p.exists():
                names |= set(pool_keys(load(p)))
    return names


def append_aliases_span(text, section, pairs):
    """Hand-formatted alias file: insert the new words at the end of the
    section, at the section's own indent, never reflowing anything else. The
    previous last entry gains the comma it was missing; the new last one
    carries none."""
    m = re.search(rf'^(\s+)"{section}": \{{', text, re.M)
    if not m:
        die(f'no "{section}" section in the alias file')
    ind = m.group(1)
    close_at = text.find(f'\n{ind}}}'  , m.end())
    if close_at < 0:
        die('malformed alias section')
    head = text[:close_at].rstrip(",")
    # the previous last line takes the comma; the new last one does not
    body = ",\n".join(f'{ind}  "{a}": "{t}"' for a, t in pairs)
    return head + ",\n" + body + f"\n{ind}}}" + text[close_at + len(f'\n{ind}}}'):]


def rebuild_aliases(doc, section, order, new):
    """Rebuild one alias section grouped by target, in order. For `sound` the
    targets are keys (the pool's own order); for `effect` they are tags (each
    tag's first appearance in the pool)."""
    groups = {}
    for alias, target in (doc.get(section) or {}).items():
        groups.setdefault(target, []).append(alias)
    for alias, target in new:
        groups.setdefault(target, []).append(alias)
    ordered = {}
    for target in order:
        for alias in groups.pop(target, []):
            ordered[alias] = target
    if groups:
        die(f"aliases left pointing outside the pool: {sorted(groups)}")
    out = {}
    for k, v in doc.items():
        if k == section:
            out[section] = ordered
        else:
            out[k] = v
    if section not in doc:
        out[section] = ordered
    return out


def main():
    args = sys.argv[1:]
    apply = False
    vals = {}
    positional = []
    i = 0
    while i < len(args):
        a = args[i]
        if a == "--apply":
            apply = True
        elif a == "--once":
            vals["once"] = True
        elif a in ("--tags", "--mode", "--hold", "--gain", "--alias", "--alias-to"):
            if i + 1 >= len(args):
                die(f"{a} needs a value")
            vals[a[2:]] = args[i + 1]
            i += 1
        else:
            positional.append(a)
        i += 1
    if len(positional) < 4:
        die("usage: add-sound.py [--apply] [--tags a,b] [--mode hit|overlap|trail]\n"
            "                   [--hold S] [--gain F] [--once] [--alias a,b] [--alias-to tag]\n"
            "                   <pack> place|moment <key> <clip> [clip...]")
    pack, level, key = positional[0], positional[1], positional[2]
    clips = [pathlib.Path(c) for c in positional[3:]]
    if level not in LUFS_TARGET:
        die(f"level must be place or moment, not {level!r}")

    pack_dir = EXTENDS / pack
    if not pack_dir.is_dir():
        die(f"no pack at {pack_dir}")
    pool_path = pack_dir / POOL_FILE[level]
    if not pool_path.exists():
        die(f"no {POOL_FILE[level]} in {pack_dir} — the registry is authored before the audio")

    print(f"{'APPLYING' if apply else 'DRY RUN'}  {pack} / {level} / {key}\n")

    pool_text = pool_path.read_text()
    doc = load(pool_path)
    existing = doc.get(key)
    tags = [t.strip() for t in vals.get("tags", "").split(",") if t.strip()]
    if existing is not None and tags and tags != existing["tags"]:
        die(f"{key}'s tags are the spec and they differ:\n"
            f"    on disk: {existing['tags']}\n    given:   {tags}")

    # ---- 1. spec ------------------------------------------------------------------------
    ready = []
    for clip in clips:
        if not clip.exists():
            die(f"no such clip: {clip}")
        ready.append(bring_to_spec(clip, level))

    # ---- 2. names, and the duplicate-recording guard --------------------------------------
    # Pending names — registry entries whose recording never landed — are the
    # family's own IOUs: the first takes fill them, and only the remainder
    # continues the numbering. Filling past them would leave -1/-2 pending
    # forever while a parallel -3/-4 ran.
    pending = []
    nums = []
    if existing is not None:
        for f in existing["files"]:
            if (pack_dir / f).exists():
                if m := re.search(rf"^{re.escape(key)}-(\d+)\.mp3$", pathlib.Path(f).name):
                    nums.append(int(m.group(1)))
            else:
                pending.append(f)
    pending.sort()
    # The numbering continues past every number the family already names —
    # on disk or pending — so a filled -1/-2 never collides with a new -3.
    all_nums = list(nums)
    for f in pending:
        if m := re.search(rf"^{re.escape(key)}-(\d+)\.mp3$", pathlib.Path(f).name):
            all_nums.append(int(m.group(1)))
    next_n = (max(all_nums) + 1) if all_nums else 1
    fill = pending[:len(ready)]
    extra = [f"{SUBDIR[level]}/{key}-{next_n + i}.mp3"
             for i in range(len(ready) - len(fill))]
    dests = fill + extra
    # A fold's `files` array already names the pending slots, so only the
    # continuation is appended; a new key holds every destination itself.
    registry_adds = extra
    if fill:
        print(f"      fills {len(fill)} pending registry slot(s): {', '.join(fill)}")
    for d in extra:
        if (pack_dir / d).exists():
            die(f"{d} already exists in {pack_dir}")
    if existing is None and not tags:
        die("a new sound needs --tags (tags are what the picker matches on)")
    # Two takes of one recording defeat the picker's second roll, which is
    # the only reason a family has more than one file.
    held = {}
    if existing is not None:
        for f in existing["files"]:
            fp = pack_dir / f
            if fp.exists():
                held[sha(fp)] = f
    seen = set()
    for src in ready:
        h = sha(src)
        if h in held:
            die(f"{src.name} is the same recording as {held[h]} — a take must be another take")
        if h in seen:
            die(f"{src.name} is given twice")
        seen.add(h)

    # ---- 3. registry plan -----------------------------------------------------------------
    kind, ind = spelling(pool_text, doc)
    if existing is None:
        entry = {"tags": tags, "files": dests}
        if level == "moment":
            entry["mode"] = vals.get("mode", "hit")
            if vals.get("hold"):
                entry["hold"] = float(vals["hold"])
            elif vals.get("mode") == "trail":
                print("      note: a trail usually wants --hold (solo seconds before the tail)")
            entry["level"] = float(vals.get("gain", 1.0))
            entry["looped"] = False
            entry["dur_s"] = max(dur_s(r) for r in ready)
        elif vals.get("once"):
            # a bed writes `looped` only when it is not the default
            entry["looped"] = False
        block = render_member(key, entry, level, kind, ind)
        new_text = insert_member(pool_text, key, block, ind)
    elif kind == "machine":
        # Rewritten whole, under the round-trip guard, BEFORE anything is
        # mutated: a file the writer does not reproduce is refused, not
        # reflowed.
        if dump_at(doc, ind) != pool_text:
            die(f"{pool_path} no longer round-trips at indent {ind}; refusing to rewrite it")
        existing["files"] += registry_adds
        longest = max(dur_s(r) for r in ready)
        if longest > existing.get("dur_s", 0):
            existing["dur_s"] = longest
        new_text = dump_at(doc, ind)
    else:
        new_text = extend_member_files(pool_text, key, ind, SUBDIR[level], registry_adds)
        if level == "moment":
            longest = max(dur_s(r) for r in ready)
            if longest > existing.get("dur_s", 0):
                new_text = extend_member_dur(new_text, key, ind, longest)

    # ---- 4. aliases -----------------------------------------------------------------------
    aliases = [a.strip() for a in vals.get("alias", "").split(",") if a.strip()]
    alias_pairs = []
    alias_text = None
    if aliases:
        taken = claimed_names()
        for a in aliases:
            if a in taken:
                die(f'alias "{a}" is already claimed elsewhere')
        if level == "moment":
            alias_pairs = [(a, key) for a in aliases]
        else:
            sound_tags = existing["tags"] if existing else tags
            target = vals.get("alias-to") or sound_tags[0]
            if target not in sound_tags:
                die(f"--alias-to {target!r} is not a tag of {key} "
                    f"(its tags: {sound_tags})")
            alias_pairs = [(a, target) for a in aliases]
        ap = pack_dir / "tag-aliases.json"
        atext = ap.read_text()
        adoc = load(ap)
        akind, aind = spelling(atext, adoc)
        if akind == "machine":
            # The order is the *planned* pool's, so a new key's aliases land
            # with it and nothing points outside.
            planned = load_text(new_text)
            if level == "moment":
                order = pool_keys(planned)
            else:
                # tags, in each tag's first-appearance order across the pool
                order = []
                for k in pool_keys(planned):
                    for t in planned[k]["tags"]:
                        if t not in order:
                            order.append(t)
            adoc = rebuild_aliases(adoc, "sound" if level == "moment" else "effect",
                                   order, alias_pairs)
            alias_text = dump_at(adoc, aind)
        else:
            # A hand-formatted alias file is appended at the section's end in
            # its own style -- grouped order is a nicety of the writer's files,
            # not worth a reflow.
            alias_text = append_aliases_span(
                atext, "sound" if level == "moment" else "effect", alias_pairs)

    # ---- report ---------------------------------------------------------------------------
    print(f"  {pack:14} {level:8} {'new key' if existing is None else 'fold into':10} {key}"
          f"  ({kind} format, indent {ind})")
    for src, dest in zip(ready, dests):
        print(f"      {src.name:28} -> {dest:36} {dur_s(src):>7}s  {sha(src)}")
    if alias_pairs:
        print(f"      aliases: {', '.join(a for a, _ in alias_pairs)}  -> {alias_pairs[0][1]}")
    if not apply:
        print("\n(dry run: nothing written)")
        return

    # ---- write -----------------------------------------------------------------------------
    pool_path.write_text(new_text)
    for src, dest in zip(ready, dests):
        target = pack_dir / dest
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(src, target)
    if alias_text is not None:
        (pack_dir / "tag-aliases.json").write_text(alias_text)
    try:
        shown = pool_path.relative_to(ROOT)
    except ValueError:  # a relocated assets root (test run)
        shown = pool_path
    print(f"\n  wrote {shown}  (+{len(dests)} file(s)"
          f"{', +' + str(len(alias_pairs)) + ' alias(es)' if alias_pairs else ''})")

    # ---- prove it ---------------------------------------------------------------------------
    inspector = ROOT / "tools" / "inspect-pool.py"
    if inspector.exists():
        subprocess.run([sys.executable, str(inspector), str(pack_dir)])

    print("\nnext, so a box ever hears it:")
    print("  ./rust/target/release/bm-inductor asset resolve --dry-run")
    print(f"  ./rust/target/release/bm-inductor profile manifest {pack} --piece pack --dep")
    print("  the next provision diffs the box's receipt and pushes only these files")


def dump_at(doc, ind):
    return json.dumps(doc, indent=ind, ensure_ascii=False) + "\n"


if __name__ == "__main__":
    main()
