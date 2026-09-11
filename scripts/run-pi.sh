#!/usr/bin/env bash
# Runs the full pipeline against the real hardware: starts pq-meter-server on
# this laptop, cross compiles pq-meter-client for the Pi, copies it over, and
# starts it there against the real meter (or a dummy one, with --dummy-meter).
#
#   scripts/run-pi.sh                 # real meter, values from CLAUDE.md
#   scripts/run-pi.sh --dummy-meter   # skip the meter, use plausible fake data
#
# Override any of these via the environment if your setup differs from
# CLAUDE.md's:
#   BIND_IP   this laptop's address the Pi can reach   (default: auto-detected)
#   PI_HOST   the gateway Pi's address                 (default: 10.175.8.132)
#   PI_USER   ssh/scp user on the Pi                    (default: anapaya)
#   METER_IP  the PQ meter's address, as seen from the Pi (default: 10.10.0.2)
#   INTERVAL  seconds between local meter samples     (default: client's own)
#
# ssh/scp use password auth (see CLAUDE.md), so this asks for the Pi's
# password once, up front, then reuses that connection (via ssh
# ControlMaster) for the copy and the remote run -- ssh-agent/keys work too,
# if you have them set up instead.
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo_root"

dummy_meter=0
case "${1:-}" in
"") ;;
--dummy-meter) dummy_meter=1 ;;
*)
  echo "usage: $0 [--dummy-meter]" >&2
  exit 2
  ;;
esac

PI_HOST=${PI_HOST:-10.175.8.132}
PI_USER=${PI_USER:-anapaya}
METER_IP=${METER_IP:-10.10.0.2}
INTERVAL=${INTERVAL:-}

if [ -z "${BIND_IP:-}" ]; then
  # Best-effort autodetection: macOS's primary Wi-Fi interface, then
  # whatever Linux's own idea of "the" address is. Neither is reliable
  # enough to trust blindly, so it's only a fallback -- BIND_IP always wins.
  BIND_IP=$(ipconfig getifaddr en0 2>/dev/null || true)
  if [ -z "$BIND_IP" ]; then
    BIND_IP=$(hostname -I 2>/dev/null | awk '{print $1}')
  fi
fi
if [ -z "$BIND_IP" ]; then
  echo "Could not autodetect this laptop's IP address. Pass the address of the" >&2
  echo "interface the Pi can reach explicitly, e.g.:" >&2
  echo "  BIND_IP=10.175.8.76 $0" >&2
  exit 2
fi

echo "==> building pq-meter-server"
cargo build -p pq-meter-server

echo "==> cross compiling pq-meter-client for the Pi (aarch64)"
cargo cross build --release -p pq-meter-client --target aarch64-unknown-linux-gnu

# Unlike run-dummy.sh, this keeps data/pqmeter.db (and the per-gateway cursor
# in it) across runs: the Pi's own pqmeter.db is never wiped either, since
# it's the real meter's accumulating history, so deleting the server's copy
# here would just force a full, ever-growing resync of that history on every
# restart instead of resuming from the cursor as designed.

log=$(mktemp "${TMPDIR:-/tmp}/pq-meter-server.XXXXXX")
control_path=$(mktemp -u "${TMPDIR:-/tmp}/pq-meter-ssh.XXXXXX")
server_pid=""
tail_pid=""
client_pid=""

cleanup() {
  for pid in "$client_pid" "$server_pid" "$tail_pid"; do
    if [ -n "$pid" ]; then
      kill "$pid" >/dev/null 2>&1 || true
      wait "$pid" 2>/dev/null || true
    fi
  done
  # Closing the shared connection hangs up the Pi's shell (and, with it, the
  # client running under it) even if killing the local ssh process above
  # somehow didn't.
  ssh -o ControlPath="$control_path" -O exit "$PI_USER@$PI_HOST" >/dev/null 2>&1 || true
  rm -f "$log"
}
trap cleanup EXIT INT TERM

# Run the built binary directly, not `cargo run`: see scripts/run-dummy.sh's
# comment on why that matters for cleanup.
echo "==> starting pq-meter-server on $BIND_IP (log: $log)"
./target/debug/pq-meter-server --bind-ip "$BIND_IP" >"$log" 2>&1 &
server_pid=$!

# The SNAP assigns the server's SCION port on every start, so it has to be
# read from the server's own output rather than hardcoded.
server_addr=""
endhost_api=""
attempt=0
while [ "$attempt" -lt 60 ]; do
  if ! kill -0 "$server_pid" 2>/dev/null; then
    echo "pq-meter-server exited before it was ready:" >&2
    cat "$log" >&2
    exit 1
  fi

  server_addr=$(sed -n 's/^  HTTP\/3 server: *//p' "$log" | head -n1)
  endhost_api=$(sed -n 's/^  gateway endhost API: *//p' "$log" | head -n1)
  if [ -n "$server_addr" ] && [ -n "$endhost_api" ]; then
    break
  fi

  sleep 0.5
  attempt=$((attempt + 1))
done

if [ -z "$server_addr" ] || [ -z "$endhost_api" ]; then
  echo "timed out waiting for pq-meter-server to print its address:" >&2
  cat "$log" >&2
  exit 1
fi

echo "    endhost API: $endhost_api"
echo "    server:      $server_addr"
echo

echo "==> opening an ssh connection to $PI_USER@$PI_HOST (password prompt below happens once)"
ssh -o ControlMaster=auto -o ControlPath="$control_path" -o ControlPersist=10m \
  -fN "$PI_USER@$PI_HOST"

echo "==> copying the client binary to the Pi"
scp -o ControlPath="$control_path" \
  target/aarch64-unknown-linux-gnu/release/pq-meter-client \
  "$PI_USER@$PI_HOST:pq-meter-client"

# Overwriting the binary file does not stop an already-running copy of it
# (Linux keeps a running executable's old inode open) -- if a client was
# started by hand before, or by an earlier run of this script that a
# ControlPersist connection outlived, kill it first so only the fresh build
# ever answers the server.
remote_cmd="pkill -x pq-meter-client >/dev/null 2>&1; sleep 0.3"
remote_cmd="$remote_cmd; chmod +x ./pq-meter-client && ./pq-meter-client"
remote_cmd="$remote_cmd --endhost-api '$endhost_api' --server '$server_addr'"
if [ "$dummy_meter" -eq 1 ]; then
  remote_cmd="$remote_cmd --dummy-meter"
else
  remote_cmd="$remote_cmd --meter-ip '$METER_IP'"
fi
if [ -n "$INTERVAL" ]; then
  remote_cmd="$remote_cmd --interval '$INTERVAL'"
fi

echo "==> starting pq-meter-client on the Pi"
echo "    NOTE: the client's own tracing lines (timestamped, INFO/WARN) are"
echo "          interleaved with the server's plain 'data: ...' /"
echo "          'received N measurement(s)...' lines, tailed live from $log."
echo "          Readings land in data/pqmeter.db; 'docker compose up -d' in a"
echo "          separate terminal serves a live dashboard of them at"
echo "          http://localhost:3000."
echo "    Press Ctrl-C to stop the server and the remote client."
echo

tail -n 0 -f "$log" &
tail_pid=$!

# Backgrounded and `wait`-ed, not run in the foreground: see
# scripts/run-dummy.sh's comment on why that's what lets Ctrl-C's trap fire
# promptly instead of only after the (reconnect-forever) client exits.
# `-tt` forces a remote pty, so hanging up this local ssh process (or the
# shared connection, in cleanup) also ends the client running under it.
ssh -tt -o ControlPath="$control_path" "$PI_USER@$PI_HOST" "$remote_cmd" &
client_pid=$!
wait "$client_pid"
