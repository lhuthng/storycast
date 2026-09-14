#!/bin/bash
# Novel swarm launcher: orchestrator + 6 pull workers, fully detached.
# Usage: ./swarm.sh [start [first count]|stop|status|logs]
#   start  kills the old digest-only session (takeover), starts swarm in tmux
#   stop   kills the swarm session (resume-safe: rerun start anytime)
#   status one-line health + mp3 count
#   logs   follow the log
set -u
cd "$(dirname "$0")"
SESSION="novel-swarm"
LOG="swarm-21-100.log"
START="${2:-21}"
COUNT="${3:-80}"

case "${1:-start}" in
  start)
    tmux kill-session -t novel-prep 2>/dev/null
    tmux kill-session -t novel-queue 2>/dev/null
    tmux kill-session -t "$SESSION" 2>/dev/null
    tmux new -d -s "$SESSION" -c "$PWD" \
      "uv run python main.py swarm --engine vieneu --start $START --count $COUNT --analyzer opencode --url-template 'https://storya.click/truyen/nguoi-tren-van-nguoi/chuong-{n}' --speed 1.25 --gap-ms 300 --ambience > $LOG 2>&1"
    sleep 15
    tmux has-session -t "$SESSION" 2>/dev/null && echo "swarm launched (tmux $SESSION, log $LOG)" || echo "LAUNCH FAILED"
    ;;
  stop)
    tmux kill-session -t "$SESSION" 2>/dev/null
    pkill -f "main.py swarm" 2>/dev/null
    sleep 1
    pgrep -f "main.py swarm" >/dev/null && echo "STILL RUNNING" || echo "swarm stopped"
    ;;
  status)
    tmux has-session -t "$SESSION" 2>/dev/null && echo "session: alive" || echo "session: dead"
    pgrep -f "main.py swarm" >/dev/null && echo "orchestrator: running" || echo "orchestrator: not running"
    echo "mp3s: $(ls output/Ch.*.mp3 2>/dev/null | wc -l | tr -d ' ')"
    tail -n 3 "$LOG" 2>/dev/null
    ;;
  logs)
    tail -f "$LOG"
    ;;
  *)
    echo "usage: $0 [start [first count]|stop|status|logs]"; exit 1 ;;
esac
