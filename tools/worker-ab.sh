#!/bin/bash
# A/B the two sidecars on this box: same runtime (1.30), same request, same voice.
#
# Written for the worker rather than the dev machine on purpose. Locally the Rust
# build links the ONNX Runtime that `ort-sys` downloads (1.28) while Python has
# 1.30, so a local comparison measures the port *and* a runtime version gap at the
# same time. Here both link the same 1.30 `.so`, which is the production pairing.
set -u

VENV="$HOME/bm-worker/python/.venv/bin/python"
PY_PORT="${PY_PORT:-8821}"
RS_PORT="${RS_PORT:-8822}"
TEXT="${TEXT:-Dịch Phong đánh giá hai nữ, hơi giật mình. Trước cửa võ quán, không một bóng người. Thôi vậy! Thanh Sơn lão tổ cũng chỉ đành chấp nhận. Hắn cười, rồi chậm rãi bước vào trong.}"

BODY=$(printf '{"text":"%s","voice":"Minh Quân","temperature":0}' "$TEXT")

echo "=== warm both ==="
for port in "$PY_PORT" "$RS_PORT"; do
  curl -s -m 300 -X POST "http://127.0.0.1:$port/infer" \
    -H 'Content-Type: application/json' -d "$BODY" -o /dev/null
done

echo "=== timing (3 rounds, warmed) ==="
psum=0
rsum=0
for i in 1 2 3; do
  p=$(curl -s -m 300 -X POST "http://127.0.0.1:$PY_PORT/infer" \
    -H 'Content-Type: application/json' -d "$BODY" -o /tmp/ab-py.wav -w '%{time_total}')
  r=$(curl -s -m 300 -X POST "http://127.0.0.1:$RS_PORT/infer" \
    -H 'Content-Type: application/json' -d "$BODY" -o /tmp/ab-rs.wav -w '%{time_total}')
  echo "  round $i: python ${p}s   rust ${r}s"
  psum=$(echo "$psum + $p" | bc)
  rsum=$(echo "$rsum + $r" | bc)
done
echo "  mean:    python $(echo "scale=3; $psum/3" | bc)s   rust $(echo "scale=3; $rsum/3" | bc)s   ratio $(echo "scale=3; $rsum/$psum" | bc)x"

echo "=== output ==="
"$VENV" - <<'PY'
import wave
import numpy as np

def rd(f):
    with wave.open(f) as w:
        return (
            np.frombuffer(w.readframes(w.getnframes()), dtype="<i2"),
            w.getframerate(),
            w.getnchannels(),
        )

a, ra, ca = rd("/tmp/ab-rs.wav")
b, rb, cb = rd("/tmp/ab-py.wav")
print(f"  rust   {a.size} samples @ {ra}Hz x{ca}, peak {abs(a).max()}")
print(f"  python {b.size} samples @ {rb}Hz x{cb}, peak {abs(b).max()}")
if a.size == b.size:
    d = np.abs(a.astype(np.int32) - b.astype(np.int32))
    print(f"  int16 diff: max {d.max()}, identical {bool((d == 0).all())}")
else:
    print("  LENGTH MISMATCH")
PY
