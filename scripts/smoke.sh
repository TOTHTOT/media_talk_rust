#!/usr/bin/env bash
# Smoke test for media_talk serve.
#
# Boots a temporary instance on 127.0.0.1:18080, polls /api/devices and opens
# a single WebSocket session (if any device + profile exists in the
# environment), and asserts we receive at least one fMP4 frame within 8s.
#
# Usage:
#   scripts/smoke.sh [bind_addr]    # default 127.0.0.1:18080
#   scripts/smoke.sh --no-serve     # just check that the binary builds + boots
#
# Environment overrides:
#   BIND      - listen address (default 127.0.0.1:18080)
#   USER / PASS - ONVIF credentials to forward
#   TIMEOUT_S - discovery / per-request timeout (default 5)
#   BINARY    - path to media_talk (default target/debug/media_talk)

set -euo pipefail

BIND="${BIND:-127.0.0.1:18080}"
USER="${USER:-}"
PASS="${PASS:-}"
TIMEOUT_S="${TIMEOUT_S:-5}"
BINARY="${BINARY:-target/debug/media_talk}"
WORKDIR="${WORKDIR:-$(mktemp -d -t mediatalk_smoke.XXXXXX)}"
LOG="${WORKDIR}/mediatalk.log"
PID_FILE="${WORKDIR}/mediatalk.pid"
SKIP_WS=0
# Allow hosts file to override via NO_PROXY for /api/devices probing.
CURL="curl --noproxy '*' -fsS"

for arg in "$@"; do
  case "$arg" in
    --no-serve) SKIP_WS=1 ;;
    --bind=*) BIND="${arg#--bind=}" ;;
  esac
done

cleanup() {
  if [[ -f "$PID_FILE" ]]; then
    local pid
    pid="$(cat "$PID_FILE" 2>/dev/null || true)"
    if [[ -n "${pid:-}" ]]; then
      kill "$pid" 2>/dev/null || true
      wait "$pid" 2>/dev/null || true
    fi
  fi
  rm -rf "$WORKDIR"
}
trap cleanup EXIT

if [[ ! -x "$BINARY" ]]; then
  echo "[smoke] building $BINARY"
  cargo build --bin media_talk --features sw-decode >/dev/null
fi

echo "[smoke] starting media_talk serve on $BIND"
auth_args=()
if [[ -n "$USER" && -n "$PASS" ]]; then
  auth_args=(--username "$USER" --password "$PASS")
fi

"$BINARY" serve --bind "$BIND" --discovery-timeout-secs "$TIMEOUT_S" \
  "${auth_args[@]}" >"$LOG" 2>&1 &
echo $! > "$PID_FILE"

# Wait for /api/devices to come up. Discovery may take ~25s on a busy
# LAN (multiple interfaces + probe retries), so poll generously.
for i in $(seq 1 200); do
  if curl --noproxy '*' -fsS "http://${BIND}/api/devices" >/dev/null 2>&1; then
    break
  fi
  sleep 0.5
done

if ! curl --noproxy '*' -fsS "http://${BIND}/api/devices" >/dev/null 2>&1; then
  echo "[smoke] server did not come up; last log:"
  tail -n 40 "$LOG" || true
  exit 1
fi
echo "[smoke] /api/devices reachable"

DEVICE_JSON="$(curl --noproxy '*' -fsS "http://${BIND}/api/devices")"
echo "[smoke] devices: $(echo "$DEVICE_JSON" | head -c 200)…"

if [[ "$SKIP_WS" -eq 1 ]]; then
  echo "[smoke] --no-serve, stopping here"
  exit 0
fi

# Try to drive a session only if there's a device+profile; otherwise exit 0
# (we still proved the server boots and exposes the API).
DEVICE_ID="$(echo "$DEVICE_JSON" | python -c "import sys,json;d=json.load(sys.stdin);print(d[0]['id'] if d else '')")"
PROFILE_ID="$(echo "$DEVICE_JSON" | python -c "import sys,json;d=json.load(sys.stdin);print((d[0]['profiles'][0]['profile_id'] if d and d[0].get('profiles') else ''))")"

if [[ -z "$DEVICE_ID" || -z "$PROFILE_ID" ]]; then
  echo "[smoke] no ONVIF devices found in the local LAN; skipping WS probe"
  exit 0
fi

echo "[smoke] creating session for device=$DEVICE_ID profile=$PROFILE_ID"
SESSION_JSON="$(curl --noproxy '*' -fsS -X POST -H "content-type: application/json" \
  -d "{\"device_id\":\"$DEVICE_ID\",\"profile_id\":\"$PROFILE_ID\"}" \
  "http://${BIND}/api/sessions")"
SESSION_ID="$(echo "$SESSION_JSON" | python -c "import sys,json;print(json.load(sys.stdin)['session_id'])")"
echo "[smoke] session_id=$SESSION_ID"

WS_URL="ws://${BIND}/ws/${SESSION_ID}"
echo "[smoke] probing $WS_URL for one fMP4 frame"

if command -v python >/dev/null 2>&1; then
  python - "$WS_URL" <<'PY'
import socket, ssl, base64, os, struct, sys, time
url = sys.argv[1]
# strip ws:// and split
assert url.startswith("ws://"), url
host_port = url[5:].split("/", 1)[0]
host, _, port = host_port.partition(":")
port = int(port or 80)
path = url[5+len(host_port):]
s = socket.create_connection((host, port), timeout=8)
key = base64.b64encode(os.urandom(16)).decode()
req = (
    f"GET {path} HTTP/1.1\r\n"
    f"Host: {host_port}\r\n"
    f"Upgrade: websocket\r\n"
    f"Connection: Upgrade\r\n"
    f"Sec-WebSocket-Key: {key}\r\n"
    f"Sec-WebSocket-Version: 13\r\n\r\n"
)
s.sendall(req.encode())
# read handshake
buf = b""
while b"\r\n\r\n" not in buf:
    chunk = s.recv(1024)
    if not chunk:
        break
    buf += chunk
if b" 101 " not in buf.split(b"\r\n", 1)[0]:
    print("[smoke] websocket upgrade failed:", buf[:200])
    sys.exit(1)
print("[smoke] ws upgrade ok, waiting for binary frame…")
s.settimeout(8)
start = time.time()
while time.time() - start < 8:
    hdr = s.recv(2)
    if len(hdr) < 2:
        continue
    fin_op = hdr[0]
    plen = hdr[1] & 0x7F
    if plen == 126:
        plen = struct.unpack("!H", s.recv(2))[0]
    elif plen == 127:
        plen = struct.unpack("!Q", s.recv(8))[0]
    payload = b""
    while len(payload) < plen:
        payload += s.recv(plen - len(payload))
    opcode = fin_op & 0x0F
    if opcode == 0x2 and payload:
        print(f"[smoke] got {len(payload)} bytes of fMP4 in {time.time()-start:.1f}s")
        sys.exit(0)
print("[smoke] timeout waiting for fMP4 frame")
sys.exit(2)
PY
  echo "[smoke] ws probe ok"
else
  echo "[smoke] python missing, skipping WS probe"
fi

echo "[smoke] done"
