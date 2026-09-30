#!/usr/bin/env python3
"""Place the prepared clips (refs/temp/clips/prep/) into their owning packs.

One-shot, like tools/make-packs.tmp.py: it records how the four packs gained
their new sounds, and it is idempotent by refusal rather than by overwriting.
Run with --apply; without it, nothing is written.

What it does, per clip:
  1. copies the prepared file into the owning pack, named `<key>-N` (takes are
     indexed from 1 -- the file index is not read by the mix, but a family that
     starts at -2 reads as if a take were missing);
  2. registers the member in that pack's pool (inject pools rewritten whole and
     guarded by a byte-exact round-trip; effect pools hand-written, so edited in
     place at the exact byte span);
  3. adds the sound aliases the prompt may guess, grouped by canonical key in
     the pool's own order, which is the order the files already use.

Every existing byte in every file it touches is preserved: the inject pools and
the alias files are only written when `dump(load(text)) == text` for the
unmodified document, and the effect pools are never re-serialized at all.
"""

import json
import hashlib
import pathlib
import re
import shutil
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
ASSETS = ROOT / "assets"
# Staging lives outside the pack: `refs/temp/` is where the normalizer's own doc
# says sources arrive, it is gitignored, and anything under `assets/` ships in
# the release (a staging folder there was 54 MB, 31% of the manifest).
PREP = ROOT / "refs" / "temp" / "clips" / "prep"

# packed => percent-encoding used by compose.rs for `member/key` records
# (LICENSES categories carry `/` and `+`), so keep keys plain.
#
# mode: `hit` owns the silence (gain 1.0), `overlap` costs no timeline time and
# runs under the following speech (gain 0.1), `trail` holds `hold` seconds then
# tails under. Every `hit` and `trail` in all three pools is level 1.0; only
# `overlap` ever trims, so `level` is written as 1.0 unless a contract
# measurement says otherwise.
CLIPS = [
    # --- weapons: the strike -----------------------------------------------------------------
    dict(pack="weapons", pool="inject", key="metal-clash",
         takes=[("metal-clash-2.mp3", 1), ("metal-clash-3.mp3", 2),
                ("metal-clash-4.mp3", 3), ("metal-clash-5.mp3", 4)],
         tags=["blade", "clang", "clash", "metal", "parry", "strike", "sword"],
         mode="hit", looped=False, level=1.0,
         aliases=["blade-clash", "steel-clash", "sword-clash", "parry"]),
    dict(pack="weapons", pool="inject", key="blunt-impact",
         takes=[("blunt-impact.mp3", 1)],
         tags=["blow", "blunt", "body", "collapse", "fall", "impact", "punch", "thud"],
         mode="hit", looped=False, level=1.0,
         aliases=["blunt-hit", "blunt-strike", "smash", "heavy-blow"]),
    dict(pack="weapons", pool="inject", key="whip-crack",
         takes=[("whip-crack.mp3", 1), ("whip-crack-2.mp3", 2)],
         tags=["crack", "lash", "snap", "strike", "whip"],
         mode="hit", looped=False, level=1.0,
         aliases=["lash", "whip"]),
    # The one whoosh in the pool (`swoosh`) is `overlap`, not `hit`: a swing
    # passes under a shout instead of owning the silence for it.
    dict(pack="weapons", pool="inject", key="air-swing",
         takes=[("air-swing.mp3", 1), ("air-swing-2.mp3", 2)],
         tags=["blade", "movement", "spear", "swing", "swoosh", "thrust", "whoosh"],
         mode="overlap", looped=False, level=1.0,
         aliases=["air-slash", "miss", "swing-miss"]),
    # --- weapons: the war din the pack's own note asks for ------------------------------------
    dict(pack="weapons", pool="effect", key="sword-war",
         takes=[("sword-war.mp3", 1)],
         tags=["army", "battle", "melee", "siege", "sword", "war"],
         looped=True),  # omitted from the file: only `looped: false` is written for a bed
    # --- common: the world --------------------------------------------------------------------
    dict(pack="common", pool="inject", key="bell",
         takes=[("bell.mp3", 1)],
         tags=["bell", "chime", "gong", "ritual", "temple"],
         mode="hit", looped=False, level=1.0,
         aliases=["bell-toll", "temple-bell", "sect-bell"]),
    # The `footstep-*` shape: a looped overlap bed the script places and stops.
    dict(pack="common", pool="inject", key="hoofbeats",
         takes=[("hoofbeats.mp3", 1)],
         tags=["gallop", "hoof", "horse", "march", "ride"],
         mode="overlap", looped=True, level=1.3,
         aliases=["hooves", "horses"]),
    # --- magic: universal arcane --------------------------------------------------------------
    dict(pack="magic", pool="inject", key="ice-spell",
         takes=[("ice-spell.mp3", 1)],
         tags=["cold", "freeze", "frost", "ice", "magic", "spell"],
         mode="hit", looped=False, level=1.0,
         aliases=["ice", "ice-magic", "frost-spell"]),
    dict(pack="magic", pool="inject", key="curse-qi",
         takes=[("curse-qi.mp3", 1), ("curse-qi-2.mp3", 2)],
         tags=["curse", "dark", "hex", "magic", "spell"],
         mode="overlap", looped=False, level=1.0,
         aliases=["curse", "hex", "dark-magic"]),
]

# Folds: prepared files that are *another take* of a key that already exists,
# rather than a key of their own. common/effect-pool.json's own note is the
# argument -- "the crickets recording is simply another night take", and
# `forest-birds` folded into `forest` the same way. A birds-only bed carrying
# `forest` would only ever race the forest bed for the same tag anyway, which is
# exactly what a take does, minus a name nothing asks for.
FOLDS = [
    dict(pack="common", pool="effect", key="forest", kind="bed",
         takes=[("birdsong.mp3", 3), ("birdsong-2.mp3", 4), ("birdsong-3.mp3", 5)],
         # forest is the only family whose files fit on one line; past ~100
         # chars the house style wraps one per line (see `night`).
         if_files_longer_than=96),
]

PACK_DIR = {"common": "common", "weapons": "weapons", "magic": "magic"}
POOL_FILE = {"inject": "inject-pool.json", "effect": "effect-pool.json"}
sub = {"inject": "injects", "effect": "effects"}


def die(msg):
    print(f"REFUSED: {msg}")
    sys.exit(1)


def load(p):
    return json.loads(p.read_text())


def sha(p):
    return hashlib.sha256(p.read_bytes()).hexdigest()[:12]


def dur_s(p):
    out = subprocess.run(
        ["ffprobe", "-v", "error", "-show_entries", "format=duration",
         "-of", "csv=p=0", str(p)],
        capture_output=True, text=True, check=True).stdout.strip()
    return round(float(out), 1)


def dump_inject(d):
    """The inject pools and the alias files are written by the Rust writer and
    reproduce exactly under indent=2 -- asserted before anything is written."""
    return json.dumps(d, indent=2, ensure_ascii=False) + "\n"


def member_text(key, entry, pool_kind):
    """One pool member in the file's own style."""
    lines = [f'  "{key}": {{']
    tags = ", ".join(f'"{t}"' for t in entry["tags"])
    files = ", ".join(f'"{f}"' for f in entry["files"])
    if pool_kind == "effect":
        lines.append(f'    "tags": [{tags}],')
        if len(f'    "files": [{files}],') > 96:
            lines.append('    "files": [')
            lines += [f'      "{f}",' for f in entry["files"]]
            lines.append("    ],")
        else:
            lines.append(f'    "files": [{files}],')
        # a bed writes `looped` only when it is not the default (see
        # `sword-fight`, the one bed in the tree that plays once)
        if not entry.get("looped", True):
            lines.append('    "looped": false')
    else:
        # one element per line, and the last one carries no comma -- JSON has no
        # trailing commas, which is what the writer's `indent=2` output shows.
        lines.append('    "tags": [')
        lines += [f'      "{t}",' for t in entry["tags"][:-1]]
        lines.append(f'      "{entry["tags"][-1]}"')
        lines.append("    ],")
        lines.append('    "files": [')
        lines += [f'      "{f}",' for f in entry["files"][:-1]]
        lines.append(f'      "{entry["files"][-1]}"')
        lines.append("    ],")
        lines.append(f'    "mode": "{entry["mode"]}",')
        if entry.get("hold") is not None:
            lines.append(f'    "hold": {entry["hold"]},')
        lines.append(f'    "level": {entry["level"]},')
        lines.append(f'    "looped": {str(entry["looped"]).lower()},')
        lines.append(f'    "dur_s": {entry["dur_s"]}')
    body = "\n".join(lines)
    return body.rstrip(",") + "\n  }"


def compare_form(entry, pool_kind):
    """What the pool would hold for this entry: exactly the fields its own
    writer emits, so a re-run can tell "already done" from "differs" -- a bed
    omits `looped` when it loops and knows no `mode`/`dur_s` at all."""
    d = {"tags": entry["tags"], "files": entry["files"]}
    if pool_kind == "effect":
        if not entry.get("looped", True):
            d["looped"] = False
        return d
    d["mode"] = entry["mode"]
    if entry.get("hold") is not None:
        d["hold"] = entry["hold"]
    d["level"] = entry["level"]
    d["looped"] = entry["looped"]
    d["dur_s"] = entry["dur_s"]
    return d


def insert_member(text, key, block):
    """Insert a top-level member, keeping every existing byte."""
    keys = [m.group(1) for m in re.finditer(r'\n  "([^"]+)": \{', text)]
    if key in keys:
        die(f'"{key}" already exists in this pool')
    later = [k for k in keys if k > key]
    if later:
        at = text.index(f'\n  "{later[0]}": {{')
        return text[:at + 1] + block + ",\n" + text[at + 1:]
    # Last member: it carries no trailing comma, so it gains one and the new
    # member goes after it. (`text[:-2]` is the document without its closing
    # `}` line, so the last member is already whole.)
    assert text.endswith("}\n"), "unexpected pool tail"
    return text[:-2].rstrip("\n") + ",\n" + block + "\n}\n"


def add_files_to_member(text, key, files):
    """Extend an existing effect member's `files`, in place."""
    m = re.search(rf'\n  "{re.escape(key)}": \{{.*?\n  \}}', text, re.S)
    if not m:
        die(f'"{key}" not found')
    member = m.group(0)
    allf = re.findall(r'"(effects/[^"]+)"', member)
    if any(f in allf for f in files):
        die(f"{files[0]} is already a take of {key}")
    merged = allf + files
    if len('    "files": [' + ", ".join(f'"{f}"' for f in merged) + "],") > 96:
            block = ('    "files": [\n'
                     + "".join(f'      "{f}",\n' for f in merged[:-1])
                     + f'      "{merged[-1]}"\n    ]')
    else:
        block = '    "files": [' + ", ".join(f'"{f}"' for f in merged) + "]"
    new_member = re.sub(r'    "files": \[.*?\]', block, member, count=1, flags=re.S)
    if new_member == member:
        die(f"could not extend {key}'s files")
    return text[:m.start()] + new_member + text[m.end():]


def alias_names():
    """Every alias name already claimed anywhere, plus every canonical sound."""
    names = set()
    for pack in ("common", "weapons", "magic"):
        d = load(ASSETS / "_extends" / PACK_DIR[pack] / "tag-aliases.json")
        for kind in ("sound", "effect", "music"):
            names |= set(d.get(kind) or {})
    for pack in ("common", "weapons", "magic"):
        for pool in ("inject", "effect"):
            p = ASSETS / "_extends" / PACK_DIR[pack] / POOL_FILE[pool]
            if p.exists():
                names |= {k for k in load(p) if k != "_note"}
    return names


def add_aliases(doc, pool_order, new):
    """Rebuild `sound` grouped by canonical key, in the pool's own order."""
    groups = {}
    for alias, target in (doc.get("sound") or {}).items():
        groups.setdefault(target, []).append(alias)
    for alias, target in new.items():
        groups.setdefault(target, []).append(alias)
    ordered = {}
    for key in pool_order:
        for alias in groups.pop(key, []):
            ordered[alias] = key
    if groups:
        die(f"aliases left pointing outside the pool: {sorted(groups)}")
    out = {}
    for k, v in doc.items():
        if k == "sound":
            out["sound"] = ordered
        else:
            out[k] = v
    if "sound" not in doc:
        out["sound"] = ordered
    return out


def main():
    apply = "--apply" in sys.argv
    print(f"{'APPLYING' if apply else 'DRY RUN'}  root={ROOT}\n")
    taken = alias_names()
    plan = []  # (pack, pool, key, [(src, dest_rel)], member, aliases)

    for c in CLIPS:
        d = ASSETS / "_extends" / PACK_DIR[c["pack"]] / POOL_FILE[c["pool"]]
        doc = load(d)
        existing = doc.get(c["key"])
        takes = []
        for src, n in c["takes"]:
            s = PREP / ("beds" if c["pool"] == "effect" else "injects") / src
            if not s.exists():
                die(f"missing prepared file {s}")
            dest = f'{sub[c["pool"]]}/{c["key"]}-{n}.mp3'
            takes.append((s, dest))
        entry = {k: v for k, v in c.items()
                 if k in ("tags", "mode", "hold", "level", "looped")}
        entry["files"] = [dest for _, dest in takes]
        entry["dur_s"] = max(dur_s(s) for s, _ in takes)
        if entry.get("hold") is None:
            entry.pop("hold", None)
        if existing is not None:
            # A re-run after an interruption: the member is already there, so
            # this loop's aliases went in with it (they are written in the same
            # step) and there is nothing left to do but check it was this clip.
            want = compare_form(entry, c["pool"])
            if existing == want:
                print(f"  {c['pack']:8} {c['pool']:6} {'skip':9} {c['key']}"
                      f"  (already registered, identical)")
                continue
            die(f'{c["pack"]}/{c["key"]} exists and differs:\n'
                f'    on disk: {json.dumps(existing, sort_keys=True)}\n'
                f'    wanted:  {json.dumps(want, sort_keys=True)}')
        for _, dest in takes:
            where = ASSETS / "_extends" / PACK_DIR[c["pack"]] / dest
            if where.exists() or (ASSETS / dest).exists():
                die(f"{dest} already exists in {c['pack']}")
        for a in c.get("aliases", []):
            if a in taken:
                die(f'alias "{a}" is already claimed elsewhere')
            taken.add(a)
        plan.append((c["pack"], c["pool"], c["key"], takes,
                     member_text(c["key"], entry, c["pool"]),
                     {a: c["key"] for a in c.get("aliases", [])}, False))

    for f in FOLDS:
        d = ASSETS / "_extends" / PACK_DIR[f["pack"]] / POOL_FILE[f["pool"]]
        doc = load(d)
        if f["key"] not in doc:
            die(f'{f["pack"]}/{f["key"]} is not registered')
        takes = []
        for src, n in f["takes"]:
            s = PREP / ("beds" if f["pool"] == "effect" else "injects") / src
            dest = f'{sub[f["pool"]]}/{f["key"]}-{n}.mp3'
            if dest in doc[f["key"]]["files"]:
                print(f"  {f['pack']:8} {f['pool']:6} {'skip':9} {f['key']}"
                      f"  ({dest} is already a take)")
                takes = []
                break
            takes.append((s, dest))
        if not takes:
            continue
        plan.append((f["pack"], f["pool"], f["key"], takes, None, {}, True))

    # ---- report ------------------------------------------------------------------------
    for pack, pool, key, takes, _, aliases, is_fold in plan:
        act = "fold into" if is_fold else "new key"
        print(f"  {pack:8} {pool:6} {act:9} {key}")
        for s, dest in takes:
            print(f"      {s.name:22} -> {dest:34} {dur_s(s):>7}s  {sha(s)}")
        if aliases:
            print(f"      aliases: {', '.join(sorted(aliases))}")
    if not apply:
        print("\n(dry run: nothing written)")
        return

    # ---- guard: every document we rewrite whole must round-trip byte-exactly --------
    for pack in ("common", "weapons", "magic"):
        for pool in ("inject", "effect"):
            p = ASSETS / "_extends" / PACK_DIR[pack] / POOL_FILE[pool]
            if pool == "inject" and dump_inject(load(p)) != p.read_text():
                die(f"{p} does not round-trip; refusing to rewrite it")
        p = ASSETS / "_extends" / PACK_DIR[pack] / "tag-aliases.json"
        if dump_inject(load(p)) != p.read_text():
            die(f"{p} does not round-trip; refusing to rewrite it")
    print("\nround-trip guards passed for every inject pool and alias file")

    # ---- write ----------------------------------------------------------------------
    for pack, pool, key, takes, member, aliases, _ in plan:
        p = ASSETS / "_extends" / PACK_DIR[pack] / POOL_FILE[pool]
        text = p.read_text()
        if member is None:  # a fold
            text = add_files_to_member(text, key, [d for _, d in takes])
        else:
            text = insert_member(text, key, member)
        p.write_text(text)
        for s, dest in takes:
            # into the *pack*, never into the resolved tree: resolve is what
            # copies a dep's files up, and a file placed in assets/ directly
            # would be withdrawn at the next resolve as an unclaimed one.
            target = ASSETS / "_extends" / PACK_DIR[pack] / dest
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(s, target)
        if aliases:
            ap = ASSETS / "_extends" / PACK_DIR[pack] / "tag-aliases.json"
            doc = load(ap)
            order = [k for k in load(p) if k != "_note"]
            ap.write_text(dump_inject(add_aliases(doc, order, aliases)))
        print(f"  wrote {p.relative_to(ROOT)}  (+{len(takes)} file(s)"
              f"{', +' + str(len(aliases)) + ' alias(es)' if aliases else ''})")

    # ---- the resolved tree is now stale on purpose ------------------------------------
    print("\nassets/_extends/ changed; the live assets/ tree is now stale:")
    print("  ./rust/target/release/bm-inductor asset resolve --dry-run")


if __name__ == "__main__":
    main()
