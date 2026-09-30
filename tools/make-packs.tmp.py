#!/usr/bin/env python3
"""Cut `weapons` and `magic` out of the live tree, and leave `xianxia` a preset.

The structure this produces:

    common ──┐
    weapons ─┼─► xianxia (a preset) ──► workspaces/<book>/assets/   deps: ["xianxia", …]
    magic ───┘                            deps: ["common","weapons","magic"]

`common` is the world; `weapons` is real arms, pre-gunpowder; `magic` is spells
and the impacts they make — a *physical* impact stays the world's. The two new
ones have no parent: they are content (pools, clips, their own rules, their own
words), the mixer knobs and the palette are `common`'s, and a pack is runnable
only when `common` is somewhere in its chain. Both start mostly empty on
purpose: a pack is something to extend.

    python3 tools/make-packs.tmp.py [--apply]
"""

import importlib.util
import json
import os
import pathlib
import shutil
import sys

APPLY = "--apply" in sys.argv
A = "assets"
C = os.path.join(A, "_extends", "common")
BEFORE = "tmp/assets-before"

_here = pathlib.Path(__file__).resolve().parent
_spec = importlib.util.spec_from_file_location("mk", _here / "make-common.tmp.py")
mk = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(mk)

WEAPONS = ["arrow-twang", "blood-spatter", "bone-crush", "explosion",
           "metal-punch", "sword-slash", "underwater-explosion"]
MAGIC = ["fire-spell", "light-spell", "lightning-spell", "water-spell"]
WEAPON_BEDS = ["sword-fight"]
# The rule whose labels are the weapons pack's own: it moves with its bed.
WEAPON_RULES = ("battle", "fight", "duel", "clash", "blade", "ambush", "war")
# Tag aliases naming those labels, which belong where the labels live.
WEAPON_TAG_ALIASES = ("blade", "combat", "fighting")

D = {name: os.path.join(A, "_extends", name) for name in ("weapons", "magic")}

NOTES = {
    "weapons": {
        "inject-pool.json":
            "Real arms: the strike, the shot, and what a blow does to what it hits."
            " Pre-gunpowder on purpose — a firearm's report is a different pack's"
            " problem — and everything a *spell* does is the magic pack's, which is"
            " why `metal-hit` and `rock-break` stay `common`'s: those are the"
            " world's impacts, not a weapon's. Content only: the beds, the mixer"
            " knobs and the palette are `common`'s, and a preset composes this pack"
            " behind itself (see the preset's `assets/pack.json`).",
        "effect-pool.json":
            "The battle bed. `looped: false` on purpose: one clash at the head of a"
            " window and the window then falls silent, because this layer is sparse"
            " by design and a fight is carried by the script's own `sword-slash`"
            " and `metal-hit` placed between lines, with the `battle` music cue"
            " under them. A sustained war din is not built.",
        "scene-map.json":
            "This pack's own rules: the one scene a weapons pack is here to score,"
            " matched ahead of every dependency weaker than this pack.",
        "tag-aliases.json":
            "The words the prompt may use for this pack's sounds and for the tags"
            " its bed carries, mapped to the canonical names in `inject-pool.json`"
            " and to the tags themselves. Applied before validation, so a synonym"
            " costs no repair round.",
        "music-pool.json":
            "Empty on purpose: the score is the genre's, not the weapon's. The"
            " palette it answers is `common`'s, in the scene map.",
    },
    "magic": {
        "inject-pool.json":
            "Spells, and the impact a spell makes — which is *not* a physical one:"
            " that is the whole reason `metal-hit`, `rock-break` and `bone-crush`"
            " are `common`'s or `weapons`' and these are not. Content only: the"
            " beds, the mixer knobs and the palette are `common`'s, and a preset"
            " composes this pack behind itself.",
        "effect-pool.json":
            "Empty, and that is a state rather than a placeholder: a spell is named"
            " by the script and lands as an *inject*, so this pack has no bed yet."
            " The first place a spell leaves behind — a formation, an array, a"
            " qi-saturated hall — is what a bed here would be, and it brings its"
            " own rule with it.",
        "scene-map.json":
            "Empty for now: a spell is an inject the script places, not a scene, so"
            " there is nothing for a rule to score yet. The first bed this pack"
            " gains is what brings its first rule.",
        "tag-aliases.json":
            "The words the prompt may use for this pack's spells, mapped to the"
            " canonical names in `inject-pool.json`. Applied before validation, so"
            " a synonym costs no repair round.",
        "music-pool.json":
            "Empty on purpose: the score is the genre's, not the spell's. The"
            " palette it answers is `common`'s, in the scene map.",
    },
}

PRESET_NOTE = (
    "This preset's own content, and there is deliberately almost none: the world"
    " is `common`'s, the arms are `weapons`', the spells are `magic`'s. What this"
    " pack *is* is the composition — see `assets/pack.json` — plus whatever taste"
    " is particular to the genre, which is what a file here is for."
)


def write(path, text):
    """Write, and refuse anything that is not parseable JSON.

    A `_note` is a string in the file, not a comment, so a stray quote in one is a
    broken registry — and nothing downstream parses a note, so a broken one fails
    silently until something reads the file whole.
    """
    try:
        json.loads(text)
    except Exception as exc:
        sys.exit(f"refusing to write {path}: {exc}")
    if APPLY:
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "w") as fh:
            fh.write(text)
    print(f"  {'wrote' if APPLY else 'would write'} {path}")


def names_in(reg, member):
    return [k for k, _, _ in mk.scan_members(reg.raw[member])]


def kept_member(reg, member, keep):
    """One member's raw text with only `keep` names, each keeping its own bytes."""
    raw = reg.raw[member]
    pairs = [(k, raw[s:e]) for k, s, e in mk.scan_members(raw) if k in keep]
    if not pairs:
        return "{}"
    body = ",\n".join(f"    {json.dumps(k)}: {v}" for k, v in pairs)
    return "{\n" + body + "\n  }"


def stash(name, rels):
    """Move clips out of the live tree into a pack's own tree."""
    for rel in rels:
        src, dst = os.path.join(A, rel), os.path.join(D[name], rel)
        if os.path.isfile(dst):
            continue  # already moved on an earlier run
        if not os.path.isfile(src):
            sys.exit(f"{src} is missing — cannot move it into {name}")
        os.makedirs(os.path.dirname(dst), exist_ok=True)
        if APPLY:
            shutil.move(src, dst)
        print(f"  {'moved' if APPLY else 'would move'} {rel} -> {name}/")


def main():
    # 1. The score: put back the tracks the music attempt withdrew. They are the
    #    genre's own files rather than a dependency's, so a copy is enough.
    src = os.path.join(BEFORE, "music")
    if not os.path.isdir(src):
        sys.exit(f"{src} is missing — no pristine copy of the tracks")
    os.makedirs(os.path.join(A, "music"), exist_ok=True)
    restored = 0
    for n in sorted(os.listdir(src)):
        dst = os.path.join(A, "music", n)
        if os.path.isfile(dst):
            continue
        if APPLY:
            shutil.copy2(os.path.join(src, n), dst)
        restored += 1
    print(f"score: {restored} track(s) {'restored' if APPLY else 'to restore'}")

    # 2. The live tree holds every entry's own bytes, wherever they came from, so
    #    the split moves text rather than re-rendering it.
    pool = mk.Registry(os.path.join(A, "inject-pool.json"))
    beds = mk.Registry(os.path.join(A, "effect-pool.json"))
    aliases = mk.Registry(os.path.join(C, "tag-aliases.json"))
    scene = mk.Registry(os.path.join(A, "scene-map.json"))
    for k in WEAPONS + MAGIC + WEAPON_BEDS:
        if k not in pool.raw and k not in beds.raw:
            sys.exit(f"{k} is in neither live pool — has this already run?")
    print(f"live: {len(pool.without_note(set(pool.keys)))} injects, "
          f"{len(beds.without_note(set(beds.keys)))} beds")

    sound_all = json.loads(aliases.raw["sound"])          # alias -> canonical name
    weapons_words = {k for k, v in sound_all.items() if v in WEAPONS + WEAPON_BEDS}
    magic_words = {k for k, v in sound_all.items() if v in MAGIC}

    # 3. The two new packs.
    for name, keys, beds_held, words in (
        ("weapons", WEAPONS, WEAPON_BEDS, weapons_words),
        ("magic", MAGIC, [], magic_words),
    ):
        notes = NOTES[name]
        write(os.path.join(D[name], "inject-pool.json"),
              pool.render([("_note", mk.quote(notes["inject-pool.json"]))]
                          + [(k, pool.raw[k]) for k in keys]))
        write(os.path.join(D[name], "effect-pool.json"),
              beds.render([("_note", mk.quote(notes["effect-pool.json"]))]
                          + [(k, beds.raw[k]) for k in beds_held]))
        write(os.path.join(D[name], "music-pool.json"),
              pool.render([("_note", mk.quote(notes["music-pool.json"]))]))

        pairs = [("_note", mk.quote(notes["tag-aliases.json"])),
                 ("sound", kept_member(aliases, "sound", words))]
        if name == "weapons":
            pairs.append(("effect", kept_member(aliases, "effect", set(WEAPON_TAG_ALIASES))))
        write(os.path.join(D[name], "tag-aliases.json"), aliases.render(pairs))

        if name == "weapons":
            raw = scene.raw["rules"]
            rules = [raw[s:e] for s, e in mk.scan_elements(raw)
                     if tuple(json.loads(raw[s:e])["match"]) == WEAPON_RULES]
            if len(rules) != 1:
                sys.exit(f"expected exactly 1 battle rule live, found {len(rules)}")
            write(os.path.join(D[name], "scene-map.json"),
                  scene.render([("_note", mk.quote(notes["scene-map.json"])),
                                ("rules", "[\n    " + ",\n    ".join(rules) + "\n  ]")]))
        else:
            write(os.path.join(D[name], "scene-map.json"),
                  scene.render([("_note", mk.quote(notes["scene-map.json"]))]))

        rels = [f for k in keys for f in json.loads(pool.raw[k]).get("files", [])]
        rels += [f for k in beds_held for f in json.loads(beds.raw[k]).get("files", [])]
        stash(name, rels)

    # 4. `common` gives up the words that went with them.
    write(aliases.path, aliases.render(
        [("_note", aliases.raw["_note"]),
         ("music", aliases.raw["music"]),
         ("effect", kept_member(aliases, "effect",
                                set(names_in(aliases, "effect")) - set(WEAPON_TAG_ALIASES))),
         ("sound", kept_member(aliases, "sound", set(sound_all) - weapons_words - magic_words))]))
    print(f"  words: {len(weapons_words) + len(magic_words)} sound alias(es) and "
          f"{len(WEAPON_TAG_ALIASES)} tag alias(es) moved out of common")

    # 5. `xianxia` becomes what it says it is: a preset.
    write(os.path.join(A, "inject-pool.json"),
          pool.render([("_note", mk.quote(PRESET_NOTE))]))
    write(os.path.join(A, "effect-pool.json"),
          beds.render([("_note", mk.quote(PRESET_NOTE))]))
    write(os.path.join(A, "scene-map.json"), scene.render(
        [("_note", mk.quote(
            "This preset's own rules, and it has none: `weapons` owns the battle"
            " scene and `common` owns the world's. A rule written here would be"
            " matched before all three, which is exactly where a preset's own taste"
            " belongs."))]))
    write(os.path.join(A, "pack.json"),
          '{\n  "_note": "This preset\'s dependencies, weakest first: `common` is the'
          ' world, `weapons` the arms, `magic` the spells. A workspace that extends'
          ' this preset names it FIRST in its own list, so the workspace\'s own files'
          ' win over it and anything it names after the preset wins over the preset.",\n'
          '  "deps": ["common", "weapons", "magic"]\n}\n')

    print("\nnext: bm-inductor asset resolve — twice, and the second must say up to date")


if __name__ == "__main__":
    main()
