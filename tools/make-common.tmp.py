#!/usr/bin/env python3
"""Cut the root `common` pack out of the live `assets/` tree.

One-shot migration, run from the repo root. `assets/` is machine-local (ignored
by git), so this exists to make the split reviewable rather than to be kept: the
shape it produces is the shape docs/ASSETS.md describes, and
`bm-inductor asset resolve` is what folds it back together.

    python3 tmp/make-common.py            # print the plan
    python3 tmp/make-common.py --apply    # move the clips and write the files

Every registry entry keeps its **raw text**: the bytes of `night` in
`common/effect-pool.json` are the bytes it had in the live file, so the merge
puts back exactly what it took out and the operator's files do not reflow.
"""

import json
import os
import shutil
import sys

APPLY = "--apply" in sys.argv
A = "assets"
D = os.path.join(A, "_extends", "common")

# --- what each side owns ----------------------------------------------------
#
# The rule the split follows: `common` is the world and the body; a genre is
# everything that only exists because of its magic. Two buckets are neither, and
# are named here so the reasoning is visible rather than implied:
#
#   * the seven combat impacts and the four spells. A spell is the genre's, but
#     there is no `weapons` or `magic` pack yet either — this script only knows
#     how to cut the root out, and `tools/make-packs.tmp.py` is what moves these
#     two buckets on to the packs that own them. They stay live here so the
#     second script has something to move.
GENRE_INJECTS = {
    "sword-slash", "arrow-twang", "bone-crush", "blood-spatter",
    "metal-punch", "explosion", "underwater-explosion",
    "fire-spell", "light-spell", "lightning-spell", "water-spell",
}
GENRE_BEDS = {"sword-fight"}
# The one rule whose labels are the genre's own (`sect`, `duel`, `war`).
GENRE_RULES = ("battle", "fight", "duel", "clash", "blade", "ambush", "war")
# The licence line that names the score, which travels with the tracks.
MUSIC_LICENCE = "background music (music/)"


# --- raw-text scanners, the ones rust/crates/bm-core/src/audio_pool.rs has ---
#
# Deliberately scanners and not parsers: the point is to recover the raw text of
# a value, which a parsed tree has already thrown away.

def _skip_string(text, i):
    n = len(text)
    i += 1
    while i < n:
        if text[i] == "\\":
            i += 2
        elif text[i] == '"':
            return i + 1
        else:
            i += 1
    raise ValueError("unterminated string")


def _skip_value(text, i):
    n = len(text)
    if text[i] == '"':
        return _skip_string(text, i)
    if text[i] in "{[":
        depth = 0
        while i < n:
            if text[i] == '"':
                i = _skip_string(text, i)
                continue
            if text[i] in "{[":
                depth += 1
            elif text[i] in "}]":
                depth -= 1
                if depth == 0:
                    return i + 1
            i += 1
        raise ValueError("unterminated value")
    while i < n and text[i] not in ",}]" and text[i] not in " \t\r\n":
        i += 1
    return i


def _ws(text, i):
    while i < len(text) and text[i] in " \t\r\n":
        i += 1
    return i


def scan_members(text):
    """[(key, value_start, value_end)] for a JSON object's top level."""
    i = _ws(text, 0)
    if text[i] != "{":
        raise ValueError("not an object")
    i += 1
    out = []
    while True:
        i = _ws(text, i)
        if text[i] == "}":
            return out
        if text[i] != '"':
            raise ValueError("expected a key")
        key_end = _skip_string(text, i)
        key = json.loads(text[i:key_end])
        i = _ws(text, key_end)
        if text[i] != ":":
            raise ValueError("expected a colon")
        i = _ws(text, i + 1)
        start = i
        end = _skip_value(text, i)
        out.append((key, start, end))
        i = _ws(text, end)
        if text[i] == ",":
            i += 1
        elif text[i] == "}":
            return out
        else:
            raise ValueError("bad separator")


def scan_elements(text):
    """[(start, end)] for a JSON array's top level."""
    i = _ws(text, 0)
    if text[i] != "[":
        raise ValueError("not an array")
    i += 1
    out = []
    while True:
        i = _ws(text, i)
        if text[i] == "]":
            return out
        start = i
        end = _skip_value(text, i)
        if end <= start:
            raise ValueError("empty element")
        out.append((start, end))
        i = _ws(text, end)
        if text[i] == ",":
            i += 1
        elif text[i] == "]":
            return out
        else:
            raise ValueError("bad separator")


class Registry:
    """A live registry, with every member's own bytes kept."""

    def __init__(self, path):
        self.path = path
        self.text = open(path).read()
        self.raw = {k: self.text[s:e] for k, s, e in scan_members(self.text)}
        self.keys = list(self.raw)

    def note(self):
        return json.loads(self.raw["_note"]) if "_note" in self.raw else ""

    def render(self, pairs):
        keys = [k for k, _ in pairs]
        # A duplicate member is not a JSON error and is a very quiet way to
        # write a file that means something else: `json.load` keeps the last
        # one, while a scanner that preserves raw text sees both.
        if len(keys) != len(set(keys)):
            raise ValueError(f"duplicate member: {keys}")
        body = ",\n".join(
            f"  {json.dumps(k, ensure_ascii=False)}: {v}" for k, v in pairs
        )
        return "{\n" + body + "\n}\n"

    def without_note(self, keys):
        """Every member in `keys`, in the order the file already has them."""
        return [(k, self.raw[k]) for k in self.keys if k in keys and k != "_note"]


def quote(text):
    return json.dumps(text, ensure_ascii=False)


def write(path, text, what=""):
    # Validate before writing: a `_note` is a string inside the file rather than
    # a comment, so one stray quote is a broken registry — and nothing
    # downstream parses a note, so a broken one fails silently.
    try:
        json.loads(text)
    except Exception as exc:
        sys.exit(f"refusing to write {path}: {exc}")
    if APPLY:
        os.makedirs(os.path.dirname(path), exist_ok=True)
        open(path, "w").write(text)
    print(f"  {'wrote' if APPLY else 'would write'} {path}{what}")


def remove(path):
    if APPLY:
        os.remove(path)
    print(f"  {'removed' if APPLY else 'would remove'} {path}")


def main():
    if os.path.isdir(D):
        sys.exit(f"{D} already exists — remove it first if you mean to redo this")
    if not os.path.isdir(A):
        sys.exit("run me from the repo root: no assets/ here")

    names = ("effect-pool.json", "inject-pool.json", "music-pool.json")
    pools = {name: Registry(os.path.join(A, name)) for name in names}
    child_keys = {
        "effect-pool.json": GENRE_BEDS,
        "inject-pool.json": GENRE_INJECTS,
    }

    def own_keys(name, reg):
        """Which entries this tree keeps, i.e. which the root does not inherit.

        The score is the exception to "the root takes the world": `common` is
        place, weather and body, and music is where a genre's identity is, so
        every track stays here and the root's registry explains the absence.
        Shipping them in the root would make them a genre's own taste wearing the
        root's name — and the mood *vocabulary* still travels, because
        `music_palette` is in the root's scene map.
        """
        if name == "music-pool.json":
            return set(reg.keys) - {"_note"}
        return child_keys[name] & set(reg.keys)

    def dep_note(name, reg):
        if name == "music-pool.json":
            return (
                "Empty on purpose. The root pack is the world — place beds, spot"
                " effects, the rules that score them, the synonym table — and music"
                " is where a genre's identity is, so the tracks and this registry"
                " are the genre's (`assets/music-pool.json`). What stays here is the"
                " mood **vocabulary**: `music_palette` in `scene-map.json` is what"
                " the prompt injects and what the digest is validated against, so a"
                " genre inherits the moods, adds its own by name, and a mood it has"
                " no track for goes silent rather than wrong."
            )
        return (
            "The root pack's: the world and the body, which every genre wants. "
            + reg.note()
        )

    def live_note(name, reg):
        if name == "music-pool.json":
            return (
                reg.note()
                + " This pack's own score. The root pack is the world — place,"
                " weather, the body — and music is where a genre's identity lives,"
                " so these tracks and this registry are not `common`'s. The moods"
                " they answer stay in `common`'s scene map (`music_palette`): that"
                " is the vocabulary the prompt injects, this is what answers it, and"
                " a mood with no track here is silent rather than wrong."
            )
        return (
            reg.note()
            + " The keys below are this pack's own; every other entry is inherited"
            " from `common` (assets/pack.json), which is where the world's sounds"
            " live. The combat impacts and the four spells stay here because there"
            " is no `combat` pack yet; they move out the day there is one."
        )

    print("== the pools ==")
    moves = []
    for name in names:
        reg = pools[name]
        mine = own_keys(name, reg)
        theirs = [k for k in reg.keys if k != "_note" and k not in mine]
        print(f"  {name}: {len(theirs)} inherited, {len(mine)} own")
        write(
            os.path.join(D, name),
            reg.render([("_note", quote(dep_note(name, reg)))]
                       + reg.without_note(set(theirs))),
        )
        write(
            reg.path,
            reg.render([("_note", quote(live_note(name, reg)))]
                       + reg.without_note(set(mine))),
            "  (trimmed to its own)",
        )
        for key in theirs:
            for rel in json.loads(reg.raw[key]).get("files", []):
                moves.append((os.path.join(A, rel), os.path.join(D, rel)))

    # --- the scene map: `rules` layers, everything else is the root's -------
    print("== the scene map ==")
    sm = Registry(os.path.join(A, "scene-map.json"))
    raw_rules = sm.raw["rules"]
    rules = [
        (raw_rules[s:e], json.loads(raw_rules[s:e])["match"])
        for s, e in scan_elements(raw_rules)
    ]
    world = [r for r, match in rules if tuple(match) != GENRE_RULES]
    genre = [r for r, match in rules if tuple(match) == GENRE_RULES]
    if len(world) + len(genre) != len(rules):
        sys.exit("a rule matched neither side of the split")
    print(f"  rules: {len(world)} inherited, {len(genre)} own")

    def rules_member(raw):
        return "[\n    " + ",\n    ".join(raw) + "\n  ]"

    write(
        os.path.join(D, "scene-map.json"),
        sm.render(
            [("_note", quote(
                "The world's rules, and the mix's knobs — the root pack's. A genre"
                " depends on this file and states only its own rules; because a"
                " rule list is matched first-to-last, an inherited rule is always"
                " seen *behind* the genre's. " + sm.note()
            )),
             ("rules", rules_member(world))]
            + [(k, v) for k, v in sm.without_note(set(sm.keys) - {"rules"})]
        ),
    )
    write(
        sm.path,
        sm.render([("_note", quote(
            "This pack's own rules, and nothing else: it depends on `common`"
            " (assets/pack.json), which supplies the world's rules, the music"
            " palette, the reverb presets and every mixer knob. A rule written here"
            " is seen FIRST — the rules are ordered specific to general and the"
            " first match wins — so a genre rule pre-empts the root's on the same"
            " scene, and restating one is how a genre overrides it."
        )),
         ("rules", rules_member(genre))]),
        "  (trimmed to its own)",
    )

    # --- the vocabulary: the root's, whole ---------------------------------
    print("== the vocabulary ==")
    reg = Registry(os.path.join(A, "tag-aliases.json"))
    write(
        os.path.join(D, "tag-aliases.json"),
        reg.render(
            [("_note", quote(
                reg.note()
                + " The vocabulary is the world's, so this file is the root pack's;"
                " a genre inherits it whole and states its own words here if it has"
                " any."
            ))]
            + reg.without_note(set(reg.keys))
        ),
    )
    remove(reg.path)

    # --- the licences: line by line, because attribution is per clip --------
    #
    # A line travels with the clips it covers, or a release ships a score with no
    # credit for it. So the score's line stays here and the rest are the root's.
    print("== the licences ==")
    lic = Registry(os.path.join(A, "LICENSES.json"))
    mine = {k for k in lic.keys if k == MUSIC_LICENCE}
    if not mine:
        sys.exit(f"no {MUSIC_LICENCE!r} line to keep — has this already run?")
    theirs = [k for k in lic.keys if k != "_note" and k not in mine]
    print(f"  {len(theirs)} inherited line(s), {len(mine)} own")
    write(
        os.path.join(D, "LICENSES.json"),
        lic.render(
            [("_note", quote(
                "Provenance for the clips in this pack, per category rather than per"
                " file. Its own lines, because its clips are the ones that travel —"
                " and attribution **layers**: a composed pack carries every line in"
                " its chain, so a pack does not restate its dependencies'."
            ))] + [(k, lic.raw[k]) for k in theirs]
        ),
    )
    write(
        lic.path,
        lic.render(
            [("_note", quote(
                lic.note()
                + " This pack's own line: its score, whose tracks are here, because"
                " music is where a genre's identity is. The sound-effects line is"
                " inherited from `common`, where those clips live."
            ))] + [(k, lic.raw[k]) for k in mine]
        ),
        "  (trimmed to its own)",
    )

    # --- the composition record --------------------------------------------
    write(
        os.path.join(A, "pack.json"),
        '{\n  "_note": "This pack\'s dependencies, weakest first. Each names a'
        ' directory under assets/_extends/; see docs/ASSETS.md. `common` is the'
        ' world — its beds, its injects, its rules, and the mix\'s knobs. The score is'
        ' not here: it belongs to the pack that extends this one. The combat'
        ' impacts and the spells are still this pack\'s own; `tools/make-packs.tmp.py`'
        ' moves them to the packs that own them.",\n'
        '  "deps": ["common"]\n}\n',
    )

    # --- and the clips ------------------------------------------------------
    print(f"== {len(moves)} clips move under {D}/ ==")
    for src, dst in moves[:3]:
        print(f"  {src}")
        print(f"    -> {dst}")
    if len(moves) > 3:
        print(f"  ... and {len(moves) - 3} more")
    if APPLY:
        for src, dst in moves:
            os.makedirs(os.path.dirname(dst), exist_ok=True)
            shutil.move(src, dst)
        print(f"  moved {len(moves)} files")
    print("\nnext: bm-inductor asset resolve --dry-run, then asset resolve")


if __name__ == "__main__":
    main()
