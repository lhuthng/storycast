"""CLI: ingest -> analyze -> synthesize. Keeps the happy path to one command."""
from __future__ import annotations

import argparse
import json
import os
import shutil
import sys
from pathlib import Path

from dotenv import load_dotenv

load_dotenv()

ENGINE = os.environ.get("TTS_ENGINE", "gemini")  # gemini | vieneu


def _engine_args(a: argparse.Namespace) -> tuple[str, str, str]:
    """Per-engine defaults: separate cast + segment cache (different voices/rates)."""
    if getattr(a, "tts_host", None):
        os.environ["TTS_HOST"] = a.tts_host
    engine = a.engine or ENGINE
    if engine == "vieneu":
        return (a.cast or "data/cast-vieneu.json",
                a.seg_dir or "data/audio/segments-vieneu", engine)
    return (a.cast or "data/cast.json",
            a.seg_dir or "data/audio/segments-gemini-v2", engine)


def _model_order(a: argparse.Namespace) -> list[str] | None:
    raw = (a.model_order or "").strip()
    return raw.replace(",", " ").split() if raw else None


def cmd_ingest(a: argparse.Namespace) -> None:
    from ingest import ingest

    ingest(url=a.url, file=a.file, out=a.out)


def cmd_analyze(a: argparse.Namespace) -> None:
    from analyze import analyze

    analyze(a.chapter, a.script, a.bible, analyzer=a.analyzer)


def cmd_synth(a: argparse.Namespace) -> None:
    from synthesize import synthesize

    cast, seg_dir, engine = _engine_args(a)
    synthesize(a.script, cast, seg_dir, a.out, limit=a.limit, dry_run=a.dry_run,
               engine=engine, model_order=_model_order(a), speed=a.speed, gap_ms=a.gap_ms,
               ambience=a.ambience)


def cmd_run(a: argparse.Namespace) -> None:
    from ingest import ingest
    from analyze import analyze
    from synthesize import synthesize

    cast, seg_dir, engine = _engine_args(a)
    ch = ingest(url=a.url, file=a.file, out=a.chapter)
    script = analyze(str(ch), a.script, analyzer=a.analyzer)
    synthesize(str(script), cast, seg_dir, a.out, limit=a.limit, dry_run=a.dry_run,
               engine=engine, model_order=_model_order(a), speed=a.speed, gap_ms=a.gap_ms,
               ambience=a.ambience)


def _chapter_paths(n: int, engine: str) -> tuple[str, str, str, str]:
    """pad, chapter txt, script json, per-chapter segment cache dir."""
    pad = f"{n:02d}"
    seg_dir = (f"data/audio/segments-{engine}-{pad}" if engine == "vieneu"
               else f"data/audio/segments-gemini-v2-{pad}")
    return pad, f"data/chapters/ch{pad}.txt", f"data/script-{pad}.json", seg_dir


def _chapter_title(chapter: str, n: int) -> str:
    try:
        first = Path(chapter).read_text(encoding="utf-8").splitlines()[0].strip()
        title = first.split(":", 1)[1] if ":" in first else first
    except (OSError, IndexError):
        title = f"Chapter {n}"
    import re as _re

    title = _re.sub(r"\s+", " ", title).strip()
    title = _re.sub(r"[\s.…]+$", "", title).strip()  # trailing dot-runs: ". . .", "..."
    title = _re.sub(r'[?:"*<>|]', "", title).strip()  # windows-illegal filename chars
    return title or f"Chapter {n}"


def cmd_prepare(a: argparse.Namespace) -> None:
    """Queue stage 1: crawl + digest N chapters into scripts. No audio."""
    from ingest import ingest
    from analyze import analyze

    ok, failed = [], []
    for n in range(a.start, a.start + a.count):
        pad, chapter, script, _ = _chapter_paths(n, "vieneu")
        if Path(script).exists():
            print(f"===== prepare chapter {n}: script exists, skipped =====")
            ok.append(n)
            continue
        print(f"===== prepare chapter {n} =====")
        try:
            if not a.skip_ingest:
                ingest(url=a.url_template.format(n=n), out=chapter)
            analyze(chapter, script, analyzer=a.analyzer)
            ok.append(n)
        except (SystemExit, Exception) as e:  # noqa: BLE001 — log and continue
            print(f"chapter {n} FAILED: {str(e)[:200]}")
            failed.append(n)
    print(f"prepare done: {len(ok)} ok {ok} | {len(failed)} failed {failed}")


def cmd_render(a: argparse.Namespace) -> None:
    """Queue stage 2: render voice segments for N scripts into per-chapter caches. No merging."""
    from synthesize import render_segments

    cast, _, engine = _engine_args(a)
    order = _model_order(a)
    ok, failed = [], []
    for n in range(a.start, a.start + a.count):
        pad, _, script, seg_dir = _chapter_paths(n, engine)
        print(f"===== voices chapter {n} =====")
        try:
            render_segments(script, cast, seg_dir, limit=a.limit,
                            dry_run=a.dry_run, engine=engine, model_order=order)
            ok.append(n)
        except (SystemExit, Exception) as e:  # noqa: BLE001 — log and continue
            print(f"chapter {n} FAILED: {str(e)[:200]}")
            failed.append(n)
    print(f"voices done: {len(ok)} ok {ok} | {len(failed)} failed {failed}")


def _merge_chapter(n: int, cast: str, engine: str, gap_ms: int = 300,
                   ambience: bool = False, speed: float = 1.0, limit: int = 0) -> Path:
    """Assemble one chapter's cached segments -> output/Ch.N - Title.mp3. No TTS."""
    from synthesize import assemble

    _, chapter, script, seg_dir = _chapter_paths(n, engine)
    tmp = f"output/.ch{n:02d}-merge.wav"
    mp3 = assemble(script, cast, seg_dir, tmp, limit=limit,
                   gap_ms=gap_ms, ambience=ambience, speed=speed,
                   engine=engine).with_suffix(".mp3")
    final = Path(f"output/Ch.{n} - {_chapter_title(chapter, n)}.mp3")
    mp3.rename(final)
    for p in Path("output").glob(f".ch{n:02d}-merge*"):
        if p != final:
            p.unlink(missing_ok=True)
    return final


def cmd_merge(a: argparse.Namespace) -> None:
    """Queue stage 3: assemble cached segments into one Ch.N - Title.mp3 per chapter. No TTS."""
    cast, _, engine = _engine_args(a)
    ok, failed = [], []
    for n in range(a.start, a.start + a.count):
        print(f"===== merge chapter {n} =====")
        try:
            print(f"done -> {_merge_chapter(n, cast, engine, a.gap_ms, a.ambience, a.speed, a.limit)}")
            ok.append(n)
        except (SystemExit, Exception) as e:  # noqa: BLE001 — log and continue
            print(f"chapter {n} FAILED: {str(e)[:200]}")
            failed.append(n)
    print(f"merge done: {len(ok)} ok {ok} | {len(failed)} failed {failed}")


def cmd_queue(a: argparse.Namespace) -> None:
    """One command, three concurrent stage workers: prepare -> render -> merge.

    Each worker independently grabs whatever is available for its stage —
    ch21 can be merging while ch40 is still digesting. The filesystem is the
    queue; one lock guards the shared bible/cast writes.
    """
    import threading

    from analyze import analyze
    from ingest import ingest
    from synthesize import assemble, segments_complete

    cast, _, engine = _engine_args(a)
    lock = threading.Lock()
    stop = threading.Event()
    chapters = list(range(a.start, a.start + a.count))
    fails: dict[tuple[str, int], int] = {}

    def paths(n: int) -> tuple[str, str, str, str]:
        return _chapter_paths(n, engine)

    def struck(stage: str, n: int) -> bool:
        return fails.get((stage, n), 0) >= 3

    def script_of(n: int) -> str:
        return paths(n)[2]

    def final_of(n: int) -> Path:
        _, chapter, _, _ = paths(n)
        return Path(f"output/Ch.{n} - {_chapter_title(chapter, n)}.mp3")

    def segs_ok(n: int) -> bool:
        _, _, script, seg = paths(n)
        return Path(script).exists() and segments_complete(script, cast, seg, engine)

    # pending: actionable right now / complete: settled (done or 3-struck upstream)
    stages = {
        "prep": (lambda n: not Path(script_of(n)).exists(),
                 lambda n: Path(script_of(n)).exists() or struck("prep", n)),
        "render": (lambda n: Path(script_of(n)).exists() and not segs_ok(n),
                   lambda n: segs_ok(n) or struck("render", n) or struck("prep", n)),
        "merge": (lambda n: segs_ok(n) and not final_of(n).exists(),
                  lambda n: final_of(n).exists() or struck("merge", n)
                  or struck("render", n) or struck("prep", n)),
    }

    def do_prep(n: int) -> None:
        _, chapter, script, _ = paths(n)
        print(f"[prep] chapter {n}", flush=True)
        if not a.skip_ingest or not Path(chapter).exists():
            ingest(url=a.url_template.format(n=n), out=chapter)
        with lock:
            analyze(chapter, script, analyzer=a.analyzer)

    def do_render(n: int) -> None:
        # Isolated process per chapter: the TTS engine's memory grows over a
        # long session, and only the OS gives it back. One chapter per process
        # bounds RAM by construction; the segment cache makes resume free.
        import subprocess

        print(f"[render] chapter {n} (isolated process)", flush=True)
        cmd = [sys.executable, str(Path(__file__).resolve()), "render",
               "--start", str(n), "--count", "1", "--engine", engine]
        if a.cast:
            cmd += ["--cast", a.cast]
        if a.model_order:
            cmd += ["--model-order", a.model_order]
        if a.tts_host:
            cmd += ["--tts-host", a.tts_host]
        try:
            subprocess.run(cmd, timeout=3600, check=True)
        except subprocess.TimeoutExpired as e:
            raise SystemExit(f"render timed out after 1h: {e}") from e
        except subprocess.CalledProcessError as e:
            raise SystemExit(f"render subprocess failed (rc={e.returncode})") from e

    def do_merge(n: int) -> None:
        _, _, script, seg = paths(n)
        print(f"[merge] chapter {n}", flush=True)
        mp3 = assemble(script, cast, seg, f"output/.ch{n:02d}-merge.wav",
                       gap_ms=a.gap_ms, ambience=a.ambience, speed=a.speed,
                       engine=engine).with_suffix(".mp3")
        final = final_of(n)
        mp3.rename(final)
        for p in Path("output").glob(f".ch{n:02d}-merge*"):
            if p != final:
                p.unlink(missing_ok=True)
        print(f"[merge] done -> {final}", flush=True)

    work = {"prep": do_prep, "render": do_render, "merge": do_merge}
    exited = {s: threading.Event() for s in stages}
    upstream = {"prep": (), "render": ("prep",), "merge": ("prep", "render")}

    def worker(name: str) -> None:
        pending, complete = stages[name]
        try:
            while not stop.is_set():
                target = next((n for n in chapters if pending(n) and not struck(name, n)), None)
                if target is None:
                    if a.once:
                        # one sweep isn't enough: downstream rescans until
                        # upstream stages are finished, then settles
                        if all(exited[u].is_set() for u in upstream[name]):
                            print(f"[{name}] settled", flush=True)
                            return
                    elif all(complete(n) for n in chapters):
                        print(f"[{name}] settled", flush=True)
                        return
                    stop.wait(a.poll)
                    continue
                try:
                    work[name](target)
                except (SystemExit, Exception) as e:  # noqa: BLE001 — count and move on
                    fails[(name, target)] = fails.get((name, target), 0) + 1
                    print(f"[{name}] chapter {target} FAILED "
                          f"({fails[(name, target)]}/3): {str(e)[:150]}", flush=True)
        finally:
            exited[name].set()

    threads = [threading.Thread(target=worker, args=(s,), daemon=True) for s in stages]
    for t in threads:
        t.start()
    try:
        for t in threads:
            t.join()
    except KeyboardInterrupt:
        print("stopping after current chapters…", flush=True)
        stop.set()
        for t in threads:
            t.join()
    bad = sorted(f"{s}:{n}" for (s, n), c in fails.items() if c >= 3)
    print(f"queue done: 3-struck {bad if bad else 'none'}")


def cmd_swarm(a: argparse.Namespace) -> None:
    """Orchestrator + 6 pull workers: prep/render/merge x local/remote.

    Nobody waits on anybody: each free worker takes the next pending task of
    its stage. Ledger is the filesystem (scripts, segment caches, mp3s, remote
    done-* markers); the orchestrator only holds claims + strike counts in
    memory. Remote lanes degrade silently on ssh failure with a 5min cooldown.
    """
    import shlex
    import subprocess
    import threading
    import time

    from analyze import analyze, load_bible, merge_bible, save_bible
    from ingest import ingest
    from synthesize import atomic_write, load_cast, segments_complete

    cast, _, engine = _engine_args(a)
    analyzer = a.analyzer or "opencode"
    urlt = a.url_template
    remote = a.remote or os.environ.get("SWARM_REMOTE", "thang@192.168.2.2")
    key = a.ssh_key or os.environ.get("SWARM_KEY", str(Path.home() / ".ssh" / "ssh-key-my-wsl"))
    use_remote = not a.no_remote
    RJOB = "~/tts-worker/job"
    OFFLINE_COOLDOWN = 300
    LEDGER = Path("data/swarm-state.json")
    LEASE = {"prep": 1200, "render": 5400, "merge": 1800}  # silent worker -> requeue, no strike

    class _RemoteDown(Exception):
        """Transport failure (ssh/box), not task failure: requeue without strike."""

    lock = threading.Lock()
    stop = threading.Event()
    chapters = list(range(a.start, a.start + a.count))
    ledger: dict[str, dict] = {}  # THE decision state: str(n) -> stage states
    state = {"off_until": 0.0}

    def _blank(n: int) -> dict:
        return {"prep": "pending", "render": "pending", "merge": "pending",
                "where": None, "strikes": {"prep": 0, "render": 0, "merge": 0},
                "lease": {"prep": 0.0, "render": 0.0, "merge": 0.0},
                "by": {"prep": None, "render": None, "merge": None}}

    def save() -> None:
        atomic_write(LEDGER, json.dumps({"chapters": ledger}, ensure_ascii=False, indent=1))

    ssh_base = ["ssh", "-i", key, "-o", "BatchMode=yes", "-o", "ConnectTimeout=15",
                "-o", "ControlMaster=auto",
                "-o", "ControlPath=" + str(Path.home() / ".ssh" / "swarm-%r-%h-%p"),
                "-o", "ControlPersist=600", remote]

    def sh(cmd: str, timeout: int = 60) -> subprocess.CompletedProcess | None:
        try:
            full = "export PATH=$HOME/.local/bin:$PATH && " + cmd
            return subprocess.run([*ssh_base, full], capture_output=True, text=True,
                                  timeout=timeout + 15)
        except Exception:  # noqa: BLE001 — timeouts/OSError = box unreachable
            return None

    def ronline() -> bool:
        if not use_remote or time.time() < state["off_until"]:
            return False
        r = sh("echo ok", timeout=15)
        if r is None or r.returncode != 0:
            state["off_until"] = time.time() + OFFLINE_COOLDOWN
            print("[swarm] remote unreachable, cooling down 5min", flush=True)
            return False
        return True

    def rsync(src: str, dst: str, timeout: int = 180) -> None:
        r = subprocess.run(["rsync", "-az", "-e", f"ssh -i {key} -o BatchMode=yes",
                            src, dst], capture_output=True, text=True, timeout=timeout)
        if r.returncode != 0:
            raise _RemoteDown(f"rsync failed: {r.stderr[-200:]}")

    def rfetch(src: str, dst_tmp: str, timeout: int = 300) -> None:
        r = subprocess.run(["scp", "-i", key, "-o", "BatchMode=yes",
                            f"{remote}:{src}", dst_tmp],
                           capture_output=True, text=True, timeout=timeout)
        if r.returncode != 0:
            raise _RemoteDown(f"fetch failed: {r.stderr[-200:]}")

    def script_of(n: int) -> Path:
        return Path(_chapter_paths(n, engine)[2])

    def final_of(n: int) -> Path:
        _, chapter, _, _ = _chapter_paths(n, engine)
        return Path(f"output/Ch.{n} - {_chapter_title(chapter, n)}.mp3")

    def segs_ok(n: int) -> bool:
        _, _, script, seg = _chapter_paths(n, engine)
        return Path(script).exists() and segments_complete(script, cast, seg, engine)

    def purge_stale(n: int) -> None:
        """Fresh digest invalidates any artifacts from older script generations."""
        import shutil as _shutil

        _, _, _, seg = _chapter_paths(n, engine)
        _shutil.rmtree(seg, ignore_errors=True)
        sh(f"rm -rf {RJOB}/done-render-{n:02d} {RJOB}/segs-{n:02d} "
           f"{RJOB}/done-merge-{n:02d} {RJOB}/Ch.{n}.mp3", timeout=30)

    # ---- lane work functions ----
    def w_prep_local(n: int) -> None:
        _, chapter, script, _ = _chapter_paths(n, engine)
        print(f"[prep:L] chapter {n}", flush=True)
        if not a.skip_ingest or not Path(chapter).exists():
            ingest(url=urlt.format(n=n), out=chapter)
        with lock:
            analyze(chapter, script, analyzer=analyzer)

    def w_prep_remote(n: int) -> None:
        pad = f"{n:02d}"
        print(f"[prep:R] chapter {n}", flush=True)
        if not ronline():
            raise _RemoteDown("remote offline")
        rsync("data/bible.json", f"{remote}:{RJOB}/data/bible.json")
        r = sh(f"cd {RJOB} && ../.venv/bin/python runjob.py prep {n} "
               f"--url-template {shlex.quote(urlt)}", timeout=900)
        if r is None:
            raise _RemoteDown("ssh lost during remote prep")
        if r.returncode != 0:
            raise SystemExit(f"remote prep failed: {r.stderr[-300:]}")
        tmp = f"data/.script-{pad}.tmp"
        rfetch(f"{RJOB}/script-{pad}.json", tmp)
        data = json.loads(Path(tmp).read_text(encoding="utf-8"))
        with lock:
            bible = load_bible()
            merge_bible(bible, data, pad)
            save_bible(bible)
        Path(tmp).replace(f"data/script-{pad}.json")
        print(f"[prep:R] chapter {n} absorbed ({len(data['segments'])} segments)", flush=True)

    def w_render_local(n: int) -> None:
        import subprocess as _sp

        print(f"[render:L] chapter {n} (isolated process)", flush=True)
        cmd = [sys.executable, str(Path(__file__).resolve()), "render",
               "--start", str(n), "--count", "1", "--engine", engine]
        if a.cast:
            cmd += ["--cast", a.cast]
        if a.model_order:
            cmd += ["--model-order", a.model_order]
        if a.tts_host:
            cmd += ["--tts-host", a.tts_host]
        try:
            _sp.run(cmd, timeout=3600, check=True)
        except _sp.TimeoutExpired as e:
            raise SystemExit(f"render timed out after 1h: {e}") from e
        except _sp.CalledProcessError as e:
            raise SystemExit(f"render subprocess failed (rc={e.returncode})") from e

    def w_render_remote(n: int) -> None:
        pad = f"{n:02d}"
        print(f"[render:R] chapter {n}", flush=True)
        if not ronline():
            raise _RemoteDown("remote offline")
        rsync(f"data/script-{pad}.json", f"{remote}:{RJOB}/script-{pad}.json")
        rsync("data/cast-vieneu.json" if engine == "vieneu" else "data/cast.json",
              f"{remote}:{RJOB}/cast.json")
        rsync("data/bible.json", f"{remote}:{RJOB}/data/bible.json")
        r = sh(f"cd {RJOB} && ../.venv/bin/python runjob.py render {n}", timeout=3600)
        if r is None:
            raise _RemoteDown("ssh lost during remote render")
        if r.returncode != 0:
            raise SystemExit(f"remote render failed: {r.stderr[-300:]}")

    def w_merge_local(n: int) -> None:
        print(f"[merge:L] chapter {n}", flush=True)
        print(f"[merge:L] done -> {_merge_chapter(n, cast, engine, a.gap_ms, a.ambience, a.speed)}",
              flush=True)

    def w_merge_remote(n: int) -> None:
        pad = f"{n:02d}"
        print(f"[merge:R] chapter {n}", flush=True)
        if not ronline():
            raise _RemoteDown("remote offline")
        r = sh(f"cd {RJOB} && ../.venv/bin/python runjob.py merge {n}", timeout=1200)
        if r is None:
            raise _RemoteDown("ssh lost during remote merge")
        if r.returncode != 0:
            raise SystemExit(f"remote merge failed: {r.stderr[-300:]}")
        tmp = f"output/.ch{pad}-rmerge.mp3"
        rfetch(f"{RJOB}/Ch.{n}.mp3", tmp)
        Path(tmp).replace(final_of(n))
        sh(f"rm -f {RJOB}/Ch.{n}.mp3", timeout=30)
        print(f"[merge:R] done -> {final_of(n)}", flush=True)

    lanes = {"prep:L": w_prep_local, "prep:R": w_prep_remote,
             "render:L": w_render_local, "render:R": w_render_remote,
             "merge:L": w_merge_local, "merge:R": w_merge_remote}

    # ---- orchestrator: the ONLY decider. Workers report; this decides. ----
    def shelved(e: dict) -> bool:
        return any(v >= 3 for v in e["strikes"].values())

    def eligible(lane: str, n: int, e: dict) -> bool:
        """May this lane take this chapter now? Pure ledger read (caller holds lock)."""
        if e[lane.split(":")[0]] != "pending" or shelved(e):
            return False
        if e["merge"] == "done":
            return False  # terminal: an mp3 already exists, no lane re-enters
        if lane.startswith("prep:"):
            return True
        if lane.startswith("render:"):
            return e["prep"] == "done"
        # merge goes where the segments are — never ships 100MB of wavs
        want = "local" if lane == "merge:L" else "remote"
        return e["render"] == "done" and e["where"] == want

    def reap() -> None:
        """Expired leases return to the pool. Silence is not failure: no strike."""
        now = time.time()
        for m in chapters:
            e = ledger[str(m)]
            for stage in ("prep", "render", "merge"):
                if e[stage] == "assigned" and e["lease"][stage] < now:
                    print(f"[swarm] lease expired: {stage} ch{m} "
                          f"(was {e['by'][stage]}), requeued", flush=True)
                    e[stage] = "pending"
                    e["by"][stage] = None
        save()

    def request(lane: str) -> int | None:
        """One task for a free worker, oldest first. The orchestrator decides."""
        with lock:
            reap()
            for n in chapters:
                if eligible(lane, n, ledger[str(n)]):
                    e = ledger[str(n)]
                    stage = lane.split(":")[0]
                    e[stage] = "assigned"
                    e["by"][stage] = lane
                    e["lease"][stage] = time.time() + LEASE[stage]
                    save()
                    return n
            return None

    def report(lane: str, n: int, ok: bool, detail: str = "",
               unreachable: bool = False) -> bool:
        """Worker reports; orchestrator transitions. False = stale report, ignored."""
        with lock:
            e = ledger[str(n)]
            stage = lane.split(":")[0]
            if e[stage] != "assigned" or e["by"][stage] != lane:
                return False  # lease expired and task moved on; result discarded
            e["by"][stage] = None
            if unreachable:
                e[stage] = "pending"
                save()
                return True
            if ok:
                e[stage] = "done"
                if stage == "render":
                    e["where"] = "remote" if lane.endswith(":R") else "local"
                if stage == "prep":
                    purge_stale(n)
                save()
                return True
            e["strikes"][stage] += 1
            e[stage] = "pending"
            save()
            if e["strikes"][stage] >= 3:
                print(f"[swarm] ch{n} SHELVED at {stage} "
                      f"({e['strikes'][stage]} strikes): {detail}", flush=True)
            return True

    def terminal(n: int) -> bool:
        e = ledger[str(n)]
        return e["merge"] == "done" or shelved(e)

    def worker(lane: str) -> None:
        while not stop.is_set():
            n = request(lane)
            if n is None:
                with lock:
                    done = all(terminal(m) for m in chapters)
                if done:
                    print(f"[{lane}] settled", flush=True)
                    return
                stop.wait(a.poll)
                continue
            try:
                lanes[lane](n)
            except _RemoteDown as e:
                with lock:
                    state["off_until"] = time.time() + OFFLINE_COOLDOWN
                report(lane, n, False, unreachable=True)
                print(f"[{lane}] remote down, ch{n} requeued (no strike): {e}", flush=True)
            except (SystemExit, Exception) as e:  # noqa: BLE001 — strike and move on
                report(lane, n, False, str(e)[:150])
                with lock:
                    c = ledger[str(n)]["strikes"][lane.split(":")[0]]
                print(f"[{lane}] chapter {n} FAILED ({c}/3): {str(e)[:150]}", flush=True)
            else:
                report(lane, n, True)

    # startup: load ledger, derive artifact truth, reconcile stale assignments
    try:
        ledger = json.loads(LEDGER.read_text(encoding="utf-8"))["chapters"]
    except (OSError, ValueError, KeyError):
        ledger = {}
    remote_markers: set[int] = set()
    if use_remote and ronline():
        print("[swarm] syncing pipeline to remote", flush=True)
        for f in ("synthesize.py", "tts_vieneu.py", "ambience.py", "analyze.py",
                  "ingest.py", "runjob.py", "prompts/analyze.txt"):
            rsync(f, f"{remote}:{RJOB}/{f}")
        r = sh(f"ls {RJOB}/done-render-*", timeout=30)
        if r is not None and r.returncode == 0:
            import re as _re

            remote_markers = {int(m) for m in _re.findall(r"done-render-(\d+)", r.stdout)}
    elif use_remote:
        print("[swarm] remote offline at start — local lanes only until it returns", flush=True)

    for n in chapters:
        e = ledger.setdefault(str(n), _blank(n))
        # artifact truth upgrades pending stages; assignments go through reconcile below
        if e["prep"] == "pending" and script_of(n).exists():
            e["prep"] = "done"
        if e["render"] == "pending" and segs_ok(n):
            e["render"] = "done"
            e["where"] = "local"
        if e["render"] == "pending":
            # stale-generation detector: cached wavs matching no expected name
            # mean the script was replaced out-of-band after rendering
            _, _, _script, _seg = _chapter_paths(n, engine)
            _dir = Path(_seg)
            if _dir.is_dir() and any(_dir.glob("*.wav")):
                from synthesize import _expected_wavs

                try:
                    _segs = json.loads(Path(_script).read_text(encoding="utf-8")).get("segments", [])
                    if engine == "vieneu":
                        import tts_vieneu as _vn

                        _cast = load_cast(_script, cast, _vn.DEFAULT_CAST, _vn.MALE_VOICES,
                                          _vn.FEMALE_VOICES, _vn.MALE_VOICES, save=False)
                    else:
                        _cast = load_cast(_script, cast, save=False)
                    _exp = {w.name for w in _expected_wavs(_segs, _cast, _seg, engine == "vieneu")}
                    _stale = {p.name for p in _dir.glob("*.wav")} - _exp
                    if _stale:
                        print(f"[swarm] ch{n}: {len(_stale)} stale-generation segments "
                              f"(script replaced after render?)", flush=True)
                except (OSError, ValueError, KeyError):
                    pass
        if e["render"] == "pending" and n in remote_markers and script_of(n).exists():
            e["render"] = "done"
            e["where"] = "remote"
        if e["merge"] == "pending" and final_of(n).exists():
            e["merge"] = "done"
        # reconcile: verify anything a dead run left assigned
        for stage in ("prep", "render", "merge"):
            if e[stage] != "assigned":
                continue
            lane = e["by"][stage] or ""
            ok = False
            if stage == "prep":
                ok = script_of(n).exists()
            elif stage == "render":
                ok = segs_ok(n)
                if not ok and lane.endswith(":R") and n in remote_markers \
                        and script_of(n).exists():
                    ok = True
                    e["where"] = "remote"
                elif ok:
                    e["where"] = "local"
            else:
                ok = final_of(n).exists()
            e[stage] = "done" if ok else "pending"
            e["by"][stage] = None
            e["lease"][stage] = 0.0
    # remote markers without a local script are stale generations: purge, prep will redo
    for n in remote_markers:
        if n in chapters and not script_of(n).exists():
            sh(f"rm -rf {RJOB}/done-render-{n:02d} {RJOB}/segs-{n:02d} "
               f"{RJOB}/done-merge-{n:02d} {RJOB}/Ch.{n}.mp3", timeout=30)
    save()
    n_prep = sum(1 for n in chapters if ledger[str(n)]["prep"] == "done")
    print(f"[swarm] ledger: {n_prep}/{len(chapters)} digested, "
          f"{sum(1 for n in chapters if terminal(n))} terminal", flush=True)

    order = ["prep:L", "prep:R", "render:L", "render:R", "merge:L", "merge:R"]
    threads = [threading.Thread(target=worker, args=(ln,), daemon=True) for ln in order]
    for t in threads:
        t.start()
    try:
        for t in threads:
            t.join()
    except KeyboardInterrupt:
        print("stopping after current tasks…", flush=True)
        stop.set()
        for t in threads:
            t.join()
    bad = sorted(f"{ledger[str(n)]['strikes']}:{n}" for n in chapters
                 if shelved(ledger[str(n)]))
    print(f"swarm done: shelved {bad if bad else 'none'}")


def cmd_batch(a: argparse.Namespace) -> None:
    """Crawl N chapters and convert each: ingest -> analyze -> synth. Bible/cast shared."""
    from ingest import ingest
    from analyze import analyze
    from synthesize import synthesize

    cast, _, engine = _engine_args(a)
    if a.seg_dir:
        print("note: --seg-dir ignored in batch mode (per-chapter caches required)")
    order = _model_order(a)
    ok, failed = [], []
    for n in range(a.start, a.start + a.count):
        pad = f"{n:02d}"
        url = a.url_template.format(n=n)
        chapter, script = f"data/chapters/ch{pad}.txt", f"data/script-{pad}.json"
        seg_dir = f"data/audio/segments-{engine}-{pad}" if engine == "vieneu" \
            else f"data/audio/segments-gemini-v2-{pad}"
        out = f"output/ch{pad}-{engine}.wav"
        print(f"===== chapter {n} =====")
        try:
            if not a.skip_ingest:
                ingest(url=url, out=chapter)
            if not a.skip_analyze:
                analyze(chapter, script, analyzer=a.analyzer)
            if not a.dry_run:
                synthesize(script, cast, seg_dir, out, limit=a.limit,
                           engine=engine, model_order=order, speed=a.speed, gap_ms=a.gap_ms,
                           ambience=a.ambience)
            ok.append(n)
        except (SystemExit, Exception) as e:  # noqa: BLE001 — log and continue with next chapter
            print(f"chapter {n} FAILED: {str(e)[:200]}")
            failed.append(n)
    print(f"batch done: {len(ok)} ok {ok} | {len(failed)} failed {failed}")


def cmd_preview(a: argparse.Namespace) -> None:
    from synthesize import preview_all

    cast, _, engine = _engine_args(a)
    preview_all(a.script, cast, a.out_dir, dry_run=a.dry_run, engine=engine)


def cmd_clone(a: argparse.Namespace) -> None:
    """Test-clone a voice from a 3-8s clip and render sample text with it."""
    import subprocess
    import tts_vieneu as vn

    tts = vn.engine()
    name = a.name or Path(a.ref).stem
    tts.add_voice(name, a.ref, denoise=not a.no_denoise)
    out = Path(a.out_dir) / f"clone-{name}.wav"
    out.parent.mkdir(parents=True, exist_ok=True)
    tts.save(tts.infer(a.text, voice=name), str(out))
    mp3 = out.with_suffix(".mp3")
    if shutil.which("ffmpeg"):
        subprocess.run(["ffmpeg", "-y", "-loglevel", "error", "-i", str(out), str(mp3)], check=True)
    print(f"cloned {a.ref} -> {mp3 if mp3.exists() else out}")


def cmd_voices(_: argparse.Namespace) -> None:
    from tts_vieneu import ALLOWED_VOICES, list_voices

    print("Central/South policy set:", sorted(ALLOWED_VOICES))
    print("--- installed roster (label -> id) ---")
    for label, vid in list_voices():
        mark = "OK " if vid in ALLOWED_VOICES else ("NORTH-skip" if "Bắc" in label else "??")
        print(f"{mark} {label} -> {vid}")


def cmd_check(_: argparse.Namespace) -> int:
    """No-API validation: prompt + code paths + JSON coherence."""
    ok = True
    for f in ("prompts/analyze.txt", "ingest.py", "analyze.py", "synthesize.py",
              "tts_vieneu.py", "tts_router.py", "ambience.py", "assets/scene-map.json",
              "pyproject.toml"):
        exists = Path(f).exists()
        print(("OK  " if exists else "MISS") + f" {f}")
        ok &= exists
    if Path("data/script-01.json").exists():
        bible_names = set()
        if Path("data/bible.json").exists():
            bible = json.loads(Path("data/bible.json").read_text(encoding="utf-8"))
            bible_names = {c["name"] for c in bible.get("characters", [])}
            print(f"bible: {len(bible_names)} characters")
        for n in range(1, 11):
            sp = f"data/script-{n:02d}.json"
            if not Path(sp).exists():
                continue
            data = json.loads(Path(sp).read_text(encoding="utf-8"))
            speakers = {s["speaker"] for s in data["segments"]}
            for cast_path in ("data/cast.json", "data/cast-vieneu.json"):
                if not Path(cast_path).exists():
                    continue
                cast = json.loads(Path(cast_path).read_text(encoding="utf-8"))
                missing = speakers - set(cast)
                nobible = speakers - bible_names - {"Narrator"} if bible_names else set()
                print(f"{sp} vs {cast_path}: covers={not missing} {sorted(missing) if missing else ''}"
                      + (f" not-in-bible={sorted(nobible)}" if nobible else ""))
                ok &= not missing
        if Path("data/cast-vieneu.json").exists():
            from tts_vieneu import ALLOWED_VOICES

            cast = json.loads(Path("data/cast-vieneu.json").read_text(encoding="utf-8"))
            bad = {k: v for k, v in cast.items() if v not in ALLOWED_VOICES}
            print(f"accent policy (Central/South only): {'OK' if not bad else bad}")
            ok &= not bad
    else:
        print("skip script/cast coherence (run analyze first)")
    print("need GEMINI_API_KEY:", bool(os.environ.get("GEMINI_API_KEY")))
    try:
        from tts_router import RPD, day_used, resolve_order

        for m in resolve_order():
            print(f"quota {m}: {day_used(m)}/{RPD} today")
    except ImportError:
        pass
    return 0 if ok else 1


def _engine_flags(p: argparse.ArgumentParser) -> None:
    p.add_argument("--engine", default=None, help="gemini | vieneu (default: $TTS_ENGINE or gemini)")
    p.add_argument("--cast", default=None, help="default: data/cast[-vieneu].json per engine")
    p.add_argument("--seg-dir", default=None, help="default: per-engine segment cache dir")
    p.add_argument("--model-order", default=None,
                   help="gemini chain override, e.g. 'gemini-2.5-pro-preview-tts gemini-2.5-flash-preview-tts'")
    p.add_argument("--speed", type=float, default=1.0, help="playback tempo on final mix (e.g. 1.5)")
    p.add_argument("--gap-ms", type=int, default=300, help="silence between lines in final mix (0 to disable)")
    p.add_argument("--tts-host", default=None, help="remote TTS worker, e.g. http://gpu-box:8818 (else $TTS_HOST)")
    p.add_argument("--ambience", action="store_true", help="per-scene beds + room reverb (needs assets/ambience/*.mp3)")


def build() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(description="Novel chapter -> multi-voice audio (Gemini digest + Gemini/VieNeu voices)")
    sub = p.add_subparsers(dest="cmd", required=True)

    i = sub.add_parser("ingest", help="fetch/clean chapter text")
    i.add_argument("--url", default=None)
    i.add_argument("--file", default=None)
    i.add_argument("--out", default="data/chapters/ch01.txt")
    i.set_defaults(fn=cmd_ingest)

    a = sub.add_parser("analyze", help="chapter txt -> script-NN.json (+ bible merge)")
    a.add_argument("--chapter", default="data/chapters/ch01.txt")
    a.add_argument("--script", default="data/script-01.json")
    a.add_argument("--bible", default="data/bible.json")
    a.add_argument("--analyzer", default=None, help="opencode | openrouter | local | gemini (default: $ANALYZER or opencode)")
    a.set_defaults(fn=cmd_analyze)

    s = sub.add_parser("synth", help="script.json -> audio")
    s.add_argument("--script", default="data/script-01.json")
    s.add_argument("--out", default="output/ch01.wav")
    s.add_argument("--limit", type=int, default=0, help="only first N segments (cheap voice test)")
    s.add_argument("--dry-run", action="store_true", help="silent wavs, no API — tests concat/plumbing")
    _engine_flags(s)
    s.set_defaults(fn=cmd_synth)

    r = sub.add_parser("run", help="ingest+analyze+synth in one go")
    r.add_argument("--url", default=None)
    r.add_argument("--file", default=None)
    r.add_argument("--chapter", default="data/chapters/ch01.txt")
    r.add_argument("--script", default="data/script-01.json")
    r.add_argument("--out", default="output/ch01.wav")
    r.add_argument("--limit", type=int, default=0)
    r.add_argument("--dry-run", action="store_true")
    r.add_argument("--analyzer", default=None, help="opencode | openrouter | local | gemini (default: $ANALYZER or opencode)")
    _engine_flags(r)
    r.set_defaults(fn=cmd_run)

    c = sub.add_parser("check", help="validate repo without spending API quota")
    c.set_defaults(fn=lambda a: sys.exit(cmd_check(a)))

    v = sub.add_parser("preview", help="one voice sample per character (check gender/fit by ear)")
    v.add_argument("--script", default="data/script-01.json")
    v.add_argument("--out-dir", default="output/voice-preview")
    v.add_argument("--dry-run", action="store_true")
    _engine_flags(v)
    v.set_defaults(fn=cmd_preview)

    vs = sub.add_parser("voices", help="list installed VieNeu presets vs Central/South policy")
    vs.set_defaults(fn=cmd_voices)

    kl = sub.add_parser("clone", help="test-clone a voice from a 3-8s clip + render sample text")
    kl.add_argument("--ref", required=True, help="reference audio (wav/mp3, 3-8s, clean, single speaker)")
    kl.add_argument("--name", default=None, help="voice name (default: clip filename)")
    kl.add_argument("--text", default="Xin chào! Giọng này được nhân bản từ đoạn mẫu, nghe có giống không nào?")
    kl.add_argument("--out-dir", default="output/voice-preview")
    kl.add_argument("--no-denoise", action="store_true", help="skip auto-denoise (clip already clean)")
    kl.set_defaults(fn=cmd_clone)

    b = sub.add_parser("batch", help="convert N chapters in one go (shared bible+cast)")
    b.add_argument("--start", type=int, required=True, help="first chapter number")
    b.add_argument("--count", type=int, required=True, help="how many chapters")
    b.add_argument("--url-template", required=True, help="e.g. https://site/truyen/x/chuong-{n}")
    b.add_argument("--limit", type=int, default=0)
    b.add_argument("--dry-run", action="store_true")
    b.add_argument("--skip-ingest", action="store_true", help="reuse existing chapter txt")
    b.add_argument("--skip-analyze", action="store_true", help="reuse existing scripts")
    b.add_argument("--analyzer", default=None, help="opencode | openrouter | local | gemini (default: $ANALYZER or opencode)")
    _engine_flags(b)
    b.set_defaults(fn=cmd_batch)

    q = sub.add_parser("prepare", help="queue stage 1: crawl+digest N chapters (no audio)")
    q.add_argument("--start", type=int, required=True)
    q.add_argument("--count", type=int, required=True)
    q.add_argument("--url-template", required=True, help="e.g. https://site/truyen/x/chuong-{n}")
    q.add_argument("--skip-ingest", action="store_true", help="reuse existing chapter txt")
    q.add_argument("--analyzer", default=None, help="opencode | openrouter | local | gemini (default: $ANALYZER or opencode)")
    q.set_defaults(fn=cmd_prepare)

    w = sub.add_parser("render", help="queue stage 2: render voice segments for N scripts (no merge)")
    w.add_argument("--start", type=int, required=True)
    w.add_argument("--count", type=int, required=True)
    w.add_argument("--limit", type=int, default=0)
    w.add_argument("--dry-run", action="store_true")
    _engine_flags(w)
    w.set_defaults(fn=cmd_render)

    m = sub.add_parser("merge", help="queue stage 3: cached segments -> Ch.N - Title.mp3 (no TTS)")
    m.add_argument("--start", type=int, required=True)
    m.add_argument("--count", type=int, required=True)
    m.add_argument("--limit", type=int, default=0)
    _engine_flags(m)
    m.set_defaults(fn=cmd_merge)

    q = sub.add_parser("queue", help="all 3 stages at once: concurrent prepare/render/merge workers")
    q.add_argument("--start", type=int, required=True)
    q.add_argument("--count", type=int, required=True)
    q.add_argument("--url-template", required=True, help="e.g. https://site/truyen/x/chuong-{n}")
    q.add_argument("--skip-ingest", action="store_true", help="reuse existing chapter txt")
    q.add_argument("--analyzer", default=None, help="opencode | openrouter | local | gemini (default: $ANALYZER or opencode)")
    q.add_argument("--once", action="store_true", help="single sweep per stage instead of waiting on upstream")
    q.add_argument("--poll", type=float, default=15, help="seconds between availability scans (default 15)")
    _engine_flags(q)
    q.set_defaults(fn=cmd_queue)

    s = sub.add_parser("swarm", help="orchestrator: 6 pull workers (prep/render/merge x local/remote)")
    s.add_argument("--start", type=int, required=True)
    s.add_argument("--count", type=int, required=True)
    s.add_argument("--url-template", required=True, help="e.g. https://site/truyen/x/chuong-{n}")
    s.add_argument("--skip-ingest", action="store_true", help="reuse existing chapter txt")
    s.add_argument("--analyzer", default=None, help="opencode for all digest workers (default: $ANALYZER or opencode)")
    s.add_argument("--remote", default=None, help="remote worker ssh target (default: $SWARM_REMOTE or thang@192.168.2.2)")
    s.add_argument("--ssh-key", default=None, help="ssh key for remote (default: $SWARM_KEY or ~/.ssh/ssh-key-my-wsl)")
    s.add_argument("--no-remote", action="store_true", help="local lanes only")
    s.add_argument("--poll", type=float, default=20, help="seconds between availability scans (default 20)")
    _engine_flags(s)
    s.set_defaults(fn=cmd_swarm)
    return p


def main() -> None:
    args = build().parse_args()
    args.fn(args)


if __name__ == "__main__":
    main()
