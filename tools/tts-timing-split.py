"""Where a render's time goes, per graph, on both engines at once.

Run it from the repo root: `.venv/bin/python tools/tts-timing-split.py`

Read `worker-ab.sh` first if the question is "how much slower is it in
production" — this script compares the *local* Rust build against the local
Python, and the local Rust build links a different ONNX Runtime version than
Python does. See the "Performance" section of the skill.


Rust reports its own split via `--timing`; this instruments the reference the same
way by wrapping each `InferenceSession.run`. The reference's `matvec` equivalent is
a NumPy `@` (which cannot be intercepted), so it lands in the remainder — which is
itself the answer, since that remainder is what BLAS is doing for free.
"""

import glob
import json
import pathlib
import re
import subprocess
import time

ROOT = pathlib.Path("/Volumes/SSD/Documents SSD/beyond-myriads-converter")
SR = 48_000
STORE = ROOT / ".venv/lib/python3.12/site-packages/vieneu/assets/voices_v3_turbo.json"
HF = pathlib.Path.home() / ".cache/huggingface/hub"
BB = sorted(glob.glob(str(HF / "models--pnnbao-ump--VieNeu-TTS-v3-Turbo/snapshots/*/onnx_update")))[-1]
CD = sorted(glob.glob(str(HF / "models--OpenMOSS-Team--MOSS-Audio-Tokenizer-Nano-ONNX/snapshots/*")))[-1]
DICT = ROOT / ".venv/lib/python3.12/site-packages/sea_g2p/sea_g2p.bin"
VOICE = "Minh Quân"

TEXTS = [
    "Không sao.",
    "Dịch Phong đánh giá hai nữ, hơi giật mình. Trước cửa võ quán, không một bóng người. "
    "Thôi vậy! Thanh Sơn lão tổ cũng chỉ đành chấp nhận. Hắn cười, rồi chậm rãi bước vào trong.",
]

from vieneu import Vieneu

times: dict[str, list[float]] = {}


def label_for(path: str) -> str:
    if "sess_pre" in path:
        return "prefill"
    if "sess_dec" in path:
        return "decode"
    if "sess_ac" in path:
        return "acoustic"
    return "codec"


def patch(sess, label: str) -> None:
    real = sess.run

    def timed(output_names, input_feed, **kw):
        t0 = time.perf_counter()
        try:
            return real(output_names, input_feed, **kw)
        finally:
            times.setdefault(label, []).append(time.perf_counter() - t0)

    sess.run = timed


def instrument(root) -> list[str]:
    found, seen, stack = [], set(), [(root, "engine")]
    while stack:
        obj, path = stack.pop()
        if id(obj) in seen or isinstance(obj, (int, float, str, bytes, list, tuple, dict)):
            continue
        seen.add(id(obj))
        if type(obj).__name__ == "InferenceSession":
            label = label_for(path)
            patch(obj, label)
            found.append(f"{path} -> {label}")
            continue
        for a in dir(obj):
            if a.startswith("__") or a.startswith("sess"):
                pass
            try:
                v = getattr(obj, a)
            except Exception:
                continue
            if type(v).__name__ == "InferenceSession":
                label = label_for(path + "." + a)
                patch(v, label)
                found.append(f"{path}.{a} -> {label}")
            elif type(v).__module__.startswith("vieneu") and not callable(v):
                stack.append((v, path + "." + a))
    return found


tts = Vieneu()
engine = getattr(tts, "engine", None) or getattr(tts, "_engine")
for line in instrument(engine):
    print("  patched", line)

for t in TEXTS:
    tts.infer(t, voice=VOICE, temperature=0.0, batch_size=1)

times.clear()
t0 = time.perf_counter()
audio_s = 0.0
for t in TEXTS:
    w = tts.infer(t, voice=VOICE, temperature=0.0, batch_size=1)
    audio_s += len(w) / SR
py_total = time.perf_counter() - t0

py = {k: (sum(v), len(v)) for k, v in times.items()}
py_sess = sum(s for s, _ in py.values())

WORK = pathlib.Path("/tmp/bm-split")
if not WORK.is_dir():
    WORK.mkdir(parents=True)
(WORK / "texts.json").write_text(json.dumps(TEXTS))

base = [
    str(ROOT / "rust/target/release/bm-tts-render"), BB,
    "--codec", CD, "--dict", str(DICT), "--voices", str(STORE),
    "--voice", VOICE, "--temp", "0",
]

# The binary reports its own per-line render time, so the model load never has to
# be subtracted — subtracting a separately-measured load was how the last attempt
# produced a render faster than the work inside it.
got = subprocess.run(
    [*base, "--texts", str(WORK / "texts.json"), "--timing"],
    capture_output=True, text=True, timeout=3600,
)
assert got.returncode == 0, got.stderr[-1500:]

rs_total = 0.0
for m in re.finditer(r"line \d+: \d+ chunks, \d+ samples \([\d.]+s\) in ([\d.]+)(ms|s)", got.stderr):
    rs_total += float(m.group(1)) / (1000.0 if m.group(2) == "ms" else 1.0)

rs: dict[str, tuple[float, int]] = {}
for line in got.stderr.splitlines():
    m = re.match(r"\s+(\w+)\s+([\d.]+) ms\s+(\d+) calls", line)
    if m:
        rs[m.group(1)] = (float(m.group(2)) / 1000.0, int(m.group(3)))
rs_sess = sum(v for k, (v, _) in rs.items() if k != "matvec")

print(f"\naudio rendered: {audio_s:.2f}s")
print(f"\n{'graph':<10} {'python':>18} {'rust':>18}   ratio")
print("-" * 62)
for key in ("prefill", "decode", "acoustic", "codec"):
    p_s, p_n = py.get(key, (0.0, 0))
    r_s, r_n = rs.get(key, (0.0, 0))
    ratio = f"{r_s / p_s:6.2f}x" if p_s else "     -"
    print(
        f"{key:<10} {p_s * 1000:9.1f} ms {p_n:>6} {r_s * 1000:9.1f} ms {r_n:>6}   {ratio}"
    )
print("-" * 62)
print(f"{'ONNX sum':<10} {py_sess * 1000:9.1f} ms {'':>6} {rs_sess * 1000:9.1f} ms {'':>6}"
      f"   {rs_sess / py_sess:6.2f}x")
print(f"{'matvec':<10} {'(in other)':>18} {rs.get('matvec', (0, 0))[0] * 1000:9.1f} ms "
      f"{rs.get('matvec', (0, 0))[1]:>6}")
print("-" * 62)
print(f"{'TOTAL':<10} {py_total * 1000:9.1f} ms {'':>6} {rs_total * 1000:9.1f} ms {'':>6}"
      f"   {rs_total / py_total:6.2f}x")
print(
    f"{'other':<10} {(py_total - py_sess) * 1000:9.1f} ms {'':>6} "
    f"{(rs_total - rs_sess - rs.get('matvec', (0, 0))[0]) * 1000:9.1f} ms"
)
print(f"\nreal-time factor: python {py_total / audio_s:.3f}x  rust {rs_total / audio_s:.3f}x")
