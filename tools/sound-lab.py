#!/usr/bin/env python3
"""A local lab: type a prompt, set the seconds, hear the clip, keep it.

    python3 tools/sound-lab.py [--port 8770] [--daemon] [--stop]

WHY THIS EXISTS. Every round of sound design so far went the same way: a prompt
was written into `docs/AUDIO-NEEDS.md`, a batch ran, and then a listening page had
to be built — inline base64, because the preview server serves one file and
cannot link its neighbours. That is a shell round trip per attempt, and an
attempt is about a second of model time. The loop was slower than the work.

So this closes it. It is the same engine, the same two flags the batch uses, and
the same negative prompt; what changes is that the length is a control instead of
a column, and the result is a player and a download link the moment it exists.

THE TWO LENGTHS ARE ONE. The model answers a request under about two seconds with
a flat block of noise, and the same prompt asked on a longer canvas comes back as
an event — and in the other direction a request past three seconds starts coming
back as a sustained wall. So `seconds` here is what the model is asked for, and
what you hear is exactly that file: nothing is cut to a row. If a clip is worth
keeping, take it through `tools/gen-sound.sh one --from` so it meets the pool on
the pool's terms (cut, fades, level, 48 kHz mono mp3).

MEDIUM RUNS ON SAME-S. The codec defaults to SAME-L for that tier, which is
another 3.4 GB for a laptop that already has the small one; `--decoder same-s`
works and keeps this a one-download install, the same choice the batch makes.

NOTHING IS INSTALLED. Clips land in `tmp/sound-lab/` with a small JSON sidecar
each (prompt, seconds, seed, model, wall time), so the history survives a reload
and a take worth keeping can be re-made from its seed. Nothing here writes to
`assets/`.

The server is read-only outside that directory, binds to loopback only, and takes
one generation at a time — the weights are 1.5-2.7 GB resident, and two at once
is how a 16 GB machine starts swapping.

`--daemon` detaches it into its own session and records the pid in
`tmp/sound-lab/server.pid`, which is how it outlives the shell that started it;
`--stop` reads that file and ends it. A lab you have to keep a terminal open for
is a lab you stop using.
"""

from __future__ import annotations

import argparse
import json
import mimetypes
import os
import pathlib
import random
import re
import signal
import subprocess
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ROOT = pathlib.Path(__file__).resolve().parent.parent
CLI = ROOT / "engines" / "stable-audio" / "src" / "optimized" / "mlx" / "sa3"
LAB = ROOT / "tmp" / "sound-lab"
STAGED = ROOT / "tmp" / "sfx-gen"
# model -> (--dit, --decoder). Both tiers here run SAME-S: it is the codec this
# machine has, and medium's default (SAME-L) is a 3.4 GB download for no gain
# while the small one is what every take so far used.
MODELS = {
    "small-sfx": ("sm-sfx", "same-s"),
    "medium": ("medium", "same-s"),
}
DEFAULT_NEGATIVE = (
    "music, background music, soundtrack, melody, instruments, drone, "
    "humming, singing, speech, voices"
)
MAX_SECONDS = 120.0
GEN_LOCK = threading.Lock()


def slug(text: str, limit: int = 40) -> str:
    return re.sub(r"[^a-z0-9]+", "-", text.lower()).strip("-")[:limit] or "clip"


def generate(prompt: str, seconds: float, model: str, seed: int,
             negative: str, cfg: float | None) -> dict:
    """Run the engine once and return what the caller needs to play the result."""
    if not CLI.exists():
        raise RuntimeError(
            f"no engine CLI at {CLI} — install the optimized runtime first "
            "(see §9 of docs/SOUND.md), or run tools/gen-sound.sh setup"
        )
    dit, decoder = MODELS[model]
    LAB.mkdir(parents=True, exist_ok=True)
    name = f"{slug(prompt)}-{seconds:g}s-seed{seed}.wav"
    dest = LAB / name
    cmd = [str(CLI), "--dit", dit, "--decoder", decoder, "--prompt", prompt,
           "--seconds", f"{seconds:g}", "--seed", str(seed), "--out", str(dest)]
    if negative:
        cmd += ["--negative-prompt", negative]
    if cfg is not None:
        cmd += ["--cfg", f"{cfg:g}"]
    started = time.time()
    # One at a time: see the module docstring. The lock is held across the whole
    # subprocess, so a second request waits rather than loading a second model.
    with GEN_LOCK:
        done = subprocess.run(cmd, stdin=subprocess.DEVNULL,
                              capture_output=True, text=True, timeout=1800)
    wall = time.time() - started
    if done.returncode != 0 or not dest.exists():
        raise RuntimeError((done.stdout or "")[-1500:] + (done.stderr or "")[-1500:])
    summary = " ".join(
        line.strip(" ▸━") for line in (done.stdout or "").splitlines()
        if "done" in line and "wall" in line
    )
    sidecar = {
        "file": name, "prompt": prompt, "seconds": seconds, "model": model,
        "dit": dit, "decoder": decoder, "seed": seed, "negative": negative,
        "cfg": cfg, "wall_s": round(wall, 2), "bytes": dest.stat().st_size,
        "created": time.strftime("%Y-%m-%d %H:%M:%S"), "summary": summary,
    }
    (LAB / f"{name}.json").write_text(json.dumps(sidecar, indent=2) + "\n")
    return sidecar


def history(limit: int = 40) -> list[dict]:
    if not LAB.is_dir():
        return []
    out = []
    for side in sorted(LAB.glob("*.json"), key=lambda p: p.stat().st_mtime, reverse=True):
        try:
            entry = json.loads(side.read_text())
        except (OSError, ValueError):
            continue
        entry["exists"] = (LAB / entry["file"]).exists()
        out.append(entry)
        if len(out) >= limit:
            break
    return out


def staged() -> dict[str, list[str]]:
    """What the batch runs have staged, so a take can be downloaded without me."""
    if not STAGED.is_dir():
        return {}
    tree: dict[str, list[str]] = {}
    for pack in sorted(p for p in STAGED.iterdir() if p.is_dir() and p.name != "_v1"):
        for f in sorted(pack.rglob("*.mp3")):
            tree.setdefault(pack.name, []).append(str(f.relative_to(ROOT)))
    if (STAGED / "_v1").is_dir():
        tree["_v1 (before the prompt rewrite)"] = [
            str(f.relative_to(ROOT)) for f in sorted((STAGED / "_v1").rglob("*.mp3"))
        ]
    return tree


PAGE = """<!doctype html><meta charset="utf-8"><title>sound lab</title>
<style>
 body{background:#14161a;color:#e6e6e6;font:15px/1.5 -apple-system,system-ui,sans-serif;
      margin:0;padding:26px 24px 80px;max-width:1000px}
 h1{font-size:21px;margin:0 0 4px} p.lede{color:#9aa0a6;margin:0 0 18px;max-width:80ch}
 p.lede b{color:#e6e6e6}
 form{background:#1a1d22;border:1px solid #2a2f36;border-radius:10px;padding:16px 18px;margin:0 0 20px}
 label{display:block;font:12px ui-monospace,Menlo,monospace;color:#9aa0a6;margin:0 0 4px}
 textarea,input,select{width:100%;box-sizing:border-box;background:#0f1115;color:#e6e6e6;
      border:1px solid #333a44;border-radius:7px;padding:9px 10px;font:14px/1.45 inherit}
 textarea{min-height:74px;resize:vertical}
 .row{display:flex;gap:12px;margin:14px 0 0;flex-wrap:wrap}
 .row>div{flex:1 1 150px}
 .quick{margin:8px 0 0}
 .quick button{margin:0 6px 0 0}
 button{background:#26303d;color:#e6e6e6;border:1px solid #3a4653;border-radius:7px;
      padding:8px 13px;font:14px inherit;cursor:pointer}
 button:hover{background:#303c4b}
 button.go{background:#2d6cdf;border-color:#3f7ce4;font-weight:600;padding:10px 18px}
 button.go[disabled]{opacity:.55;cursor:progress}
 .result{background:#1e2733;border-left:3px solid #8ab4f8;border-radius:9px;padding:12px 14px;
      margin:0 0 22px;display:none}
 .result.on{display:block}
 .result.bad{background:#2a2020;border-left-color:#b3524a}
 .meta{font:11.5px/1.6 ui-monospace,Menlo,monospace;color:#b6bcc4;margin:6px 0 0}
 .meta b{color:#e6e6e6} .meta span{color:#8ab4f8;word-break:break-all}
 audio{width:100%;margin:7px 0 4px;height:34px}
 a.dl{display:inline-block;margin:4px 10px 0 0;color:#8ab4f8;font:13px ui-monospace,Menlo,monospace}
 h2{font-size:15px;color:#8ab4f8;margin:26px 0 10px;font-family:ui-monospace,Menlo,monospace}
 .hist{border-top:1px solid #232830;padding:9px 0 4px}
 .hist .who{font:12px ui-monospace,Menlo,monospace;color:#c9d1d9}
 .hist .when{font:11.5px ui-monospace,Menlo,monospace;color:#8b93a0}
 details{margin:0 0 10px} summary{cursor:pointer;color:#8ab4f8;font:13px ui-monospace,Menlo,monospace}
 details a{display:block;font:11.5px/1.7 ui-monospace,Menlo,monospace;color:#b6bcc4;
      text-decoration:none} details a:hover{color:#e6e6e6}
 .warn{color:#e6c07b;font:12px ui-monospace,Menlo,monospace}
</style>
<h1>sound lab</h1>
<p class="lede">One clip at a time, from the same engine and the same negative prompt the batch uses.
<b>seconds</b> is what the model is asked for, and the file you get is exactly that — nothing is cut to a row.
Under about two seconds the model answers with a block of noise instead of an event, and past roughly three
it can come back as a sustained wall, so try a prompt at two lengths before judging it. Nothing here touches
<code>assets/</code>; clips land in <code>tmp/sound-lab/</code>.</p>

<form id="f">
  <label for="prompt">prompt — an action, its direction, and what the material does</label>
  <textarea id="prompt">a heavy curtain pulled aside along its rail, the fabric sliding and gathering to one side</textarea>
  <div class="row">
    <div><label for="seconds">seconds</label><input id="seconds" type="number" value="3" min="0.5" max="120" step="0.1"></div>
    <div><label for="model">model</label><select id="model"><option>small-sfx</option><option>medium</option></select></div>
    <div><label for="seed">seed (blank = random)</label><input id="seed" type="text" placeholder=""></div>
    <div><label for="cfg">cfg (blank = default)</label><input id="cfg" type="text" placeholder=""></div>
  </div>
  <div class="quick">
    <button type="button" data-s="2">2 s</button><button type="button" data-s="3">3 s</button>
    <button type="button" data-s="4">4 s</button><button type="button" data-s="8">8 s</button>
    <button type="button" data-s="30">30 s</button><button type="button" data-s="60">60 s</button>
  </div>
  <div class="row"><div><label for="negative">negative prompt</label><input id="negative" type="text"></div></div>
  <div class="row"><div><button class="go" id="go" type="submit">generate</button>
    <span class="warn" id="hint"></span></div></div>
</form>

<div class="result" id="result"></div>
<h2 id="hcount">this session</h2>
<div id="history"></div>
<h2>staged takes</h2>
<p class="lede" id="staged-note"></p>
<div id="staged"></div>

<script>
const $ = id => document.getElementById(id);
$("negative").value = %NEGATIVE%;

function human(b){ return b > 1048576 ? (b/1048576).toFixed(1)+" MB" : Math.round(b/1024)+" KB"; }
function esc(s){ const d = document.createElement("div"); d.textContent = s; return d.innerHTML; }

function clip_card(e){
  const url = "/file?path=" + encodeURIComponent("tmp/sound-lab/" + e.file);
  const bits = [`<b>${e.seconds}s</b>`, `${e.model}`, `seed <b>${e.seed}</b>`,
                `${e.wall_s}s to make`, `${human(e.bytes)}`];
  if (e.cfg !== null && e.cfg !== undefined) bits.push(`cfg ${e.cfg}`);
  return `<div class="result on"><div class="who">${esc(e.created || "")}</div>
    <audio controls preload="metadata" src="${url}"></audio>
    <div class="meta">${bits.join(" · ")}</div>
    <div class="meta"><span>${esc(e.prompt)}</span></div>
    <div class="meta">${esc(e.summary || "")}</div>
    <a class="dl" href="${url}&download=1" download>download wav</a>
    <a class="dl" href="#" data-prompt="${esc(e.prompt)}" data-seconds="${e.seconds}"
       data-seed="${e.seed}" data-model="${e.model}">load into the form</a></div>`;
}

function render_history(list){
  $("hcount").textContent = list.length ? `history (${list.length})` : "history";
  $("history").innerHTML = list.map(clip_card).join("");
  document.querySelectorAll("[data-prompt]").forEach(a => a.onclick = ev => {
    ev.preventDefault();
    $("prompt").value = a.dataset.prompt; $("seconds").value = a.dataset.seconds;
    $("seed").value = a.dataset.seed; $("model").value = a.dataset.model;
    window.scrollTo({top:0, behavior:"smooth"});
  });
}

$("f").addEventListener("submit", async ev => {
  ev.preventDefault();
  const body = {
    prompt: $("prompt").value.trim(),
    seconds: parseFloat($("seconds").value),
    model: $("model").value,
    seed: $("seed").value.trim() === "" ? null : parseInt($("seed").value, 10),
    negative: $("negative").value.trim(),
    cfg: $("cfg").value.trim() === "" ? null : parseFloat($("cfg").value),
  };
  if (!body.prompt) { $("hint").textContent = "a prompt, first"; return; }
  $("go").disabled = true; $("hint").textContent = "generating…";
  const t0 = Date.now();
  const tick = setInterval(() => $("hint").textContent = `generating… ${((Date.now()-t0)/1000).toFixed(0)}s`, 500);
  try {
    const r = await fetch("/api/generate", {method:"POST", headers:{"content-type":"application/json"},
                                           body: JSON.stringify(body)});
    const j = await r.json();
    if (!j.ok) throw new Error(j.error || "generation failed");
    $("seed").value = "";
    render_history([j.clip, ...window.__hist]);
    window.__hist = [j.clip, ...window.__hist];
    $("hint").textContent = "";
  } catch (err) {
    const box = $("result");
    box.className = "result on bad";
    box.innerHTML = `<div class="who">failed</div><div class="meta">${esc(String(err.message)).slice(0,4000)}</div>`;
    $("hint").textContent = "";
  } finally {
    clearInterval(tick);
    $("go").disabled = false;
  }
});

document.querySelectorAll("[data-s]").forEach(b => b.onclick = () => $("seconds").value = b.dataset.s);

(async () => {
  window.__hist = await (await fetch("/api/history")).json();
  render_history(window.__hist);
  const tree = await (await fetch("/api/staged")).json();
  const packs = Object.keys(tree);
  $("staged-note").textContent = packs.length
    ? packs.map(p => `${p} (${tree[p].length})`).join(" · ") + " — click to open, click a file to download it."
    : "nothing staged yet (tools/gen-sound.sh batch writes here)";
  $("staged").innerHTML = packs.map(p => `<details><summary>${esc(p)} — ${tree[p].length} files</summary>` +
    tree[p].map(f => `<a href="/file?path=${encodeURIComponent(f)}&download=1" download>${esc(f.split("/").pop())}</a>`).join("") +
    `</details>`).join("");
})();
</script>
"""


class Lab(BaseHTTPRequestHandler):
    server_version = "sound-lab"

    def log_message(self, fmt, *args):          # one line a request, no timestamps
        print(f"  {fmt % args}")

    def _send(self, code: int, body: bytes, ctype: str, extra: dict | None = None):
        self.send_response(code)
        self.send_header("content-type", ctype)
        self.send_header("content-length", str(len(body)))
        for k, v in (extra or {}).items():
            self.send_header(k, v)
        self.end_headers()
        if self.command != "HEAD":
            self.wfile.write(body)

    def _json(self, code: int, obj) -> None:
        self._send(code, json.dumps(obj).encode(), "application/json")

    def _under_tmp(self, rel: str) -> pathlib.Path | None:
        """A path the request may read: inside tmp/, and an audio file.

        The lab hands out what it made, not what it can reach. `resolve()` on both
        sides is what stops `../..` from walking out of tmp/.
        """
        try:
            p = (ROOT / rel).resolve()
            p.relative_to((ROOT / "tmp").resolve())
        except (ValueError, OSError):
            return None
        return p if p.is_file() and p.suffix.lower() in (".wav", ".mp3", ".json") else None

    def do_GET(self):
        path, _, query = self.path.partition("?")
        q = dict(kv.split("=", 1) for kv in query.split("&") if "=" in kv)
        if path == "/":
            page = PAGE.replace("%NEGATIVE%", json.dumps(DEFAULT_NEGATIVE))
            return self._send(200, page.encode(), "text/html; charset=utf-8")
        if path == "/api/history":
            return self._json(200, history())
        if path == "/api/staged":
            return self._json(200, staged())
        if path == "/file":
            from urllib.parse import unquote
            target = self._under_tmp(unquote(q.get("path", "")))
            if target is None:
                return self._json(404, {"error": "not a file under tmp/"})
            ctype = mimetypes.guess_type(target.name)[0] or "application/octet-stream"
            extra = {"content-disposition": f'attachment; filename="{target.name}"'} if q.get("download") else {}
            return self._send(200, target.read_bytes(), ctype, extra)
        self._json(404, {"error": "no such route"})

    def do_POST(self):
        if self.path != "/api/generate":
            return self._json(404, {"error": "no such route"})
        length = int(self.headers.get("content-length") or 0)
        try:
            req = json.loads(self.rfile.read(length) or b"{}")
        except ValueError:
            return self._json(400, {"ok": False, "error": "body must be json"})
        prompt = str(req.get("prompt") or "").strip()[:1000]
        model = str(req.get("model") or "small-sfx")
        negative = str(req.get("negative") if req.get("negative") is not None else DEFAULT_NEGATIVE)[:400]
        try:
            seconds = float(req.get("seconds") or 0)
        except (TypeError, ValueError):
            seconds = 0.0
        seed = req.get("seed")
        cfg = req.get("cfg")
        if not prompt:
            return self._json(400, {"ok": False, "error": "a prompt is required"})
        if model not in MODELS:
            return self._json(400, {"ok": False, "error": f"model must be one of {list(MODELS)}"})
        if not (0.5 <= seconds <= MAX_SECONDS):
            return self._json(400, {"ok": False, "error": f"seconds must be 0.5-{MAX_SECONDS:g}"})
        if not isinstance(seed, int) or isinstance(seed, bool):
            seed = random.randint(0, 2**31 - 1)
        if cfg is not None:
            try:
                cfg = float(cfg)
            except (TypeError, ValueError):
                cfg = None
        try:
            clip = generate(prompt, seconds, model, seed, negative, cfg)
        except subprocess.TimeoutExpired:
            return self._json(504, {"ok": False, "error": "the engine did not finish in 30 minutes"})
        except Exception as err:                       # surfaced to the page as text
            return self._json(500, {"ok": False, "error": str(err)})
        print(f"  made {clip['file']}  ({clip['seconds']:g}s, {clip['model']}, seed {clip['seed']}, "
              f"{clip['wall_s']}s)")
        return self._json(200, {"ok": True, "clip": clip})


def daemonize(pidfile: pathlib.Path) -> None:
    """Detach into a new session, then log to a file and record the pid.

    The double fork is the point of it: the first child leaves the caller's
    process group, and the second is reparented to init so no shell teardown can
    reach it. Whatever started this can walk away.
    """
    if os.fork() > 0:
        os._exit(0)
    os.setsid()
    if os.fork() > 0:
        os._exit(0)
    os.umask(0o022)
    log = os.open(str(LAB / "server.log"), os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o644)
    null = os.open(os.devnull, os.O_RDONLY)
    os.dup2(null, 0)
    os.dup2(log, 1)
    os.dup2(log, 2)
    pidfile.write_text(f"{os.getpid()}\n")


def stop(pidfile: pathlib.Path) -> None:
    if not pidfile.exists():
        raise SystemExit("no pidfile — nothing to stop")
    pid = int(pidfile.read_text().strip() or 0)
    try:
        os.kill(pid, signal.SIGTERM)
        print(f"stopped pid {pid}")
    except ProcessLookupError:
        print(f"pid {pid} was already gone")
    pidfile.unlink(missing_ok=True)


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--port", type=int, default=8770)
    ap.add_argument("--host", default="127.0.0.1",
                    help="loopback only, on purpose: this runs a model and reads the disk")
    ap.add_argument("--daemon", action="store_true",
                    help="detach into the background; the pid goes in tmp/sound-lab/server.pid")
    ap.add_argument("--stop", action="store_true",
                    help="stop the server named by that pidfile")
    args = ap.parse_args()
    LAB.mkdir(parents=True, exist_ok=True)
    pidfile = LAB / "server.pid"

    if args.stop:
        return stop(pidfile)

    if pidfile.exists():
        old = int(pidfile.read_text().strip() or 0)
        try:
            os.kill(old, 0)
            alive = True
        except ProcessLookupError:
            alive = False
        if alive:
            raise SystemExit(f"already running as pid {old} — `--stop` first, or pick another --port")
        pidfile.unlink(missing_ok=True)

    if not CLI.exists():
        print(f"no engine CLI at {CLI}\n"
              "install the optimized runtime (see §9 of docs/SOUND.md) — the lab will not start "
              "without something to run", flush=True)
        raise SystemExit(1)

    if args.daemon:
        daemonize(pidfile)
    httpd = ThreadingHTTPServer((args.host, args.port), Lab)
    print(f"sound lab  http://{args.host}:{args.port}/", flush=True)
    print(f"  engine   {CLI}", flush=True)
    print(f"  clips    {LAB}", flush=True)
    print("  one generation at a time; ctrl-c to stop", flush=True)
    try:
        httpd.serve_forever()
    except KeyboardInterrupt:
        print("\nstopped")
        pidfile.unlink(missing_ok=True)


if __name__ == "__main__":
    main()
