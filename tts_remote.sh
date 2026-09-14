#!/bin/bash
# Remote TTS worker lifecycle (single model process at a time — the box has 1.9GB RAM).
# Usage: ./tts_remote.sh {start|stop|status|logs|enroll}
# enroll = stop worker -> enroll refs/*.wav voices -> save -> start worker -> verify.
set -u
KEY="$HOME/.ssh/ssh-key-my-wsl"
HOST="thang@192.168.2.2"
DIR="\$HOME/tts-worker"
ssh="ssh -i $KEY -o ConnectTimeout=15 $HOST"

remote() { $ssh "$1"; }

case "${1:-status}" in
  start)
    remote "cd $DIR && (tmux has-session -t tts 2>/dev/null || tmux new -d -s tts './.venv/bin/python tts_server.py --port 8818 --bind 0.0.0.0') && sleep 3 && curl -s --max-time 10 http://127.0.0.1:8818/health || echo STARTED-NO-HEALTH-YET"
    ;;
  stop)
    remote "tmux kill-session -t tts 2>/dev/null; pkill -f tts_server.py 2>/dev/null; echo stopped"
    ;;
  status)
    remote "tmux has-session -t tts 2>/dev/null && echo 'tmux: up' || echo 'tmux: down'; free -m | sed -n 2p; curl -s --max-time 5 http://127.0.0.1:8818/health || echo 'health: DOWN'"
    ;;
  logs)
    remote "tmux capture-pane -p -t tts | tail -n ${2:-20}"
    ;;
  enroll)
    # One model process at a time: stop worker first, enroll, restart, verify.
    remote "tmux kill-session -t tts 2>/dev/null; pkill -f tts_server.py 2>/dev/null; sleep 2; cd $DIR && ./.venv/bin/python -c \"
import tts_vieneu as vn
tts = vn.engine()
import glob
for ref in sorted(glob.glob('refs/*.wav')):
    name = __import__('pathlib').Path(ref).stem.split('-')[0].capitalize()
    tts.add_voice(name, ref)
    print('enrolled', name, flush=True)
tts.save_voices()
print('saved', flush=True)
\" && tmux new -d -s tts './.venv/bin/python tts_server.py --port 8818 --bind 0.0.0.0' && sleep 5 && curl -s --max-time 10 http://127.0.0.1:8818/voices"
    ;;
  *)
    echo "usage: $0 {start|stop|status|logs|enroll}"; exit 1 ;;
esac
