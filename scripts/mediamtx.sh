#!/usr/bin/env bash
# Run MediaMTX with tests/mediamtx/mediamtx.yml on free loopback ports, by hand.
#
#   scripts/mediamtx.sh start    # start, wait until its API answers, print the ports
#   scripts/mediamtx.sh status   # print the ports and whether the recorded process is alive
#   scripts/mediamtx.sh stop     # stop the recorded process (by PID) and clean up
#
# MediaMTX is found through $MEDIAMTX_BIN, else `mediamtx` on PATH; this script downloads
# nothing. Without one it prints why and exits 3 (the Rust harness skips the same way).
# State lives in $MEDIAMTX_STATE (default: a per-user dir under $TMPDIR): the PID, the ports
# (env file) and the log. Output of `start`:
#
#   WHIP_URL=http://127.0.0.1:PORT/<path>/whip   (replace <path> with any stream name)
#   WHEP_URL=http://127.0.0.1:PORT/<path>/whep
#   API_URL=http://127.0.0.1:PORT
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
config="$root/tests/mediamtx/mediamtx.yml"
state=${MEDIAMTX_STATE:-${TMPDIR:-/tmp}/simee-mediamtx-$(id -u)}
pidfile="$state/pid"
envfile="$state/ports.env"
logfile="$state/mediamtx.log"

find_bin() {
  if [[ -n ${MEDIAMTX_BIN:-} ]]; then
    [[ -x $MEDIAMTX_BIN ]] && printf '%s\n' "$MEDIAMTX_BIN"
    return 0
  fi
  command -v mediamtx || true
}

# Ports nothing listens on right now: bind port 0 and read it back, as many as asked for.
free_ports() {
  python3 - "$1" <<'PY'
import socket, sys
socks = []
for _ in range(int(sys.argv[1])):
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    socks.append(s)
print(*[s.getsockname()[1] for s in socks])
PY
}

alive() {
  [[ -f $pidfile ]] && kill -0 "$(cat "$pidfile")" 2>/dev/null
}

print_urls() {
  local webrtc_port api_port
  webrtc_port=$(sed -n 's/^WEBRTC_PORT=//p' "$envfile")
  api_port=$(sed -n 's/^API_PORT=//p' "$envfile")
  echo "WHIP_URL=http://127.0.0.1:$webrtc_port/<path>/whip"
  echo "WHEP_URL=http://127.0.0.1:$webrtc_port/<path>/whep"
  echo "API_URL=http://127.0.0.1:$api_port"
  echo "PID=$(cat "$pidfile")"
  echo "LOG=$logfile"
}

start() {
  local bin
  bin=$(find_bin)
  if [[ -z $bin ]]; then
    echo "mediamtx: not available (set MEDIAMTX_BIN or put mediamtx on PATH); nothing to start" >&2
    return 3
  fi
  if alive; then
    echo "mediamtx: already running (pid $(cat "$pidfile"))" >&2
    print_urls
    return 0
  fi
  mkdir -p "$state"
  local ports webrtc_port api_port media_port
  ports=$(free_ports 3)
  read -r webrtc_port api_port media_port <<<"$ports"
  printf 'WEBRTC_PORT=%s\nAPI_PORT=%s\nMEDIA_PORT=%s\n' "$webrtc_port" "$api_port" "$media_port" >"$envfile"
  MTX_WEBRTCADDRESS="127.0.0.1:$webrtc_port" \
    MTX_APIADDRESS="127.0.0.1:$api_port" \
    MTX_WEBRTCLOCALUDPADDRESS="127.0.0.1:$media_port" \
    MTX_WEBRTCLOCALTCPADDRESS="127.0.0.1:$media_port" \
    "$bin" "$config" >"$logfile" 2>&1 &
  echo $! >"$pidfile"
  local i
  for ((i = 0; i < 100; i++)); do
    if ! alive; then
      echo "mediamtx: exited during startup; log:" >&2
      tail -n 20 "$logfile" >&2
      rm -f "$pidfile"
      return 1
    fi
    if curl -fsS -o /dev/null "http://127.0.0.1:$api_port/v3/config/global/get" 2>/dev/null; then
      print_urls
      return 0
    fi
    sleep 0.1
  done
  echo "mediamtx: API not ready after 10 s; log:" >&2
  tail -n 20 "$logfile" >&2
  stop
  return 1
}

stop() {
  if alive; then
    local pid i
    pid=$(cat "$pidfile")
    kill "$pid" 2>/dev/null || true
    for ((i = 0; i < 50; i++)); do
      kill -0 "$pid" 2>/dev/null || break
      sleep 0.1
    done
    kill -0 "$pid" 2>/dev/null && kill -9 "$pid" 2>/dev/null || true
  fi
  rm -f "$pidfile" "$envfile"
}

status() {
  if alive; then
    print_urls
  else
    echo "mediamtx: not running"
    return 1
  fi
}

case "${1:-}" in
  start) start ;;
  stop) stop ;;
  status) status ;;
  *)
    echo "usage: scripts/mediamtx.sh start|stop|status" >&2
    exit 2
    ;;
esac
