#!/bin/bash
# One-off: merge chapters 1-41 (for their cue sidecars) then render the acts.
set -u
cd "/Volumes/SSD/Documents SSD/beyond-myriads-converter" || exit 1
BIN=./rust/target/debug/bm-agent
OUT=workspaces/beyond-myriads/output

echo "=== $(date) merge phase ==="
for n in $(seq 1 41); do
  # Version 2 is one cue per script segment; an older sidecar would caption the
  # book a different way, so it does not count as present.
  if grep -lq '"version": 2' "$OUT"/Ch."$n"\ -\ *.cues.json 2>/dev/null; then
    echo "ch$n: cues present"
    continue
  fi
  if $BIN run --stage merge --chapter "$n" --engine vieneu >/dev/null 2>&1; then
    echo "ch$n: merged"
  else
    echo "ch$n: MERGE FAILED"
  fi
done

echo "=== $(date) render phase ==="
.venv/bin/python3 tools/video.py --acts renders/acts.json
echo "=== $(date) DONE rc=$? ==="
