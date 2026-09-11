#!/usr/bin/env bash
# Runs pq-meter-client against its dummy meter (see crates/pq-meter-client/src/meter.rs).
#
#   scripts/run-dummy.sh                hermetic: build and run the client's tests,
#                                        printing a real NDJSON reply. No network needed.
#   scripts/run-dummy.sh --live         start pq-meter-server, scrape its (randomly
#                                        assigned) SCION address from its output, and
#                                        point the client at it.
#   scripts/run-dummy.sh --live --web   the above, plus the Angular dashboard
#                                        (`web/`) at http://localhost:4200, reading
#                                        live data through its dev proxy. Installs
#                                        `web/node_modules` first if missing.
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo_root"

live=0
web=0
for arg in "$@"; do
  case "$arg" in
  --live) live=1 ;;
  --web) web=1 ;;
  *)
    echo "usage: $0 [--live] [--web]" >&2
    exit 2
    ;;
  esac
done
if [ "$web" -eq 1 ] && [ "$live" -eq 0 ]; then
  echo "--web needs --live too: the dashboard has nothing to show without a running server" >&2
  exit 2
fi

if [ "$live" -eq 0 ]; then
  echo "==> building pq-meter-client"
  cargo build -p pq-meter-client

  echo
  echo "==> running the client's tests (the 'dummy data reply: ...' line below is"
  echo "    a real reply the client produced over an in-memory tunnel, no server needed)"
  echo
  cargo test -p pq-meter-client -- --nocapture
  exit 0
fi

echo "==> building pq-meter-server and pq-meter-client"
cargo build -p pq-meter-server -p pq-meter-client

# Both the client's and the server's SQLite files are transient/inspectable
# local state, not something a fresh demo run should replay. Left over from
# a previous run, the server's stored cursor for this gateway would be
# ahead of what the client's fresh history can ever reach again -- it
# self-heals (see CONNECT_PROTOCOL.md's "Gateway Identity and History
# Resets"), but only after detecting the mismatch on the first pull, so
# starting clean avoids the extra 5s delay and any confusion from stale
# rows briefly still being what a dashboard shows.
echo "==> removing any pqmeter.db/data/pqmeter.db left over from a previous run"
rm -f pqmeter.db pqmeter.db-shm pqmeter.db-wal
rm -f data/pqmeter.db data/pqmeter.db-shm data/pqmeter.db-wal

log=$(mktemp "${TMPDIR:-/tmp}/pq-meter-server.XXXXXX")
web_log=""
server_pid=""
tail_pid=""
web_pid=""
web_tail_pid=""
client_pid=""

cleanup() {
  for pid in "$client_pid" "$server_pid" "$web_pid" "$tail_pid" "$web_tail_pid"; do
    if [ -n "$pid" ]; then
      kill "$pid" >/dev/null 2>&1 || true
      wait "$pid" 2>/dev/null || true
    fi
  done
  rm -f "$log" "$web_log"
}
trap cleanup EXIT INT TERM

# Run the built binary directly, not `cargo run`: cargo would spawn the
# server as its own child, so `$!` below would be cargo's PID, and killing
# it on cleanup would orphan the server holding the fixed endhost API port.
echo "==> starting pq-meter-server (log: $log)"
./target/debug/pq-meter-server >"$log" 2>&1 &
server_pid=$!

# The SNAP assigns the server's SCION port on every start, so it has to be
# read from the server's own output rather than hardcoded. Poll the log file
# instead of piping the server into a read loop: Rust's stdout stays
# line-buffered even when redirected, so no `stdbuf`/`script` wrapper is
# needed (and macOS ships neither by default).
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

if [ "$web" -eq 1 ]; then
  if [ ! -d "$repo_root/web/node_modules" ]; then
    echo "==> installing web frontend dependencies (first run only)"
    (cd "$repo_root/web" && npm install)
  fi

  # `ng serve`'s dev proxy (web/proxy.conf.json) forwards /edh/v1/* to the
  # server's plain-HTTP data API on the fixed port web_api::WEB_API_PORT
  # (31030) -- unlike server_addr above, nothing here needs to be scraped.
  web_log=$(mktemp "${TMPDIR:-/tmp}/pq-meter-web.XXXXXX")
  echo "==> starting the web frontend (ng serve, log: $web_log)"
  (cd "$repo_root/web" && npm start) >"$web_log" 2>&1 &
  web_pid=$!
fi

echo "NOTE: the client below opens a CONNECT tunnel and answers the server's"
echo "      dummy-meter data requests over it (see CONNECT_PROTOCOL.md)."
echo "      Below, the client's own tracing lines (timestamped, INFO/WARN)"
echo "      are interleaved with the server's plain 'data: ...' /"
echo "      'received N measurement(s)...' lines, tailed live from $log."
echo "      Readings land in data/pqmeter.db; 'docker compose up -d' in a"
echo "      separate terminal serves a Grafana dashboard of them at"
echo "      http://localhost:3000."
if [ "$web" -eq 1 ]; then
  echo "      The Angular dashboard is compiling at http://localhost:4200 --"
  echo "      give it a few seconds, its own log is tailed live from $web_log."
fi
echo "      Press Ctrl-C to stop everything started above."
echo

# Tail only what the server (and, with --web, the frontend) prints from here
# on -- their startup lines were already surfaced above. A plain `tail -f`,
# not piped through anything, so `tail_pid`/`web_tail_pid` below are the
# actual processes to kill on cleanup: piping through e.g. `sed` for a line
# prefix would make `$!` the pid of that downstream command instead, and
# killing only that leaves `tail` an orphan once its log stops growing (it's
# blocked in a read, so it never gets SIGPIPE from the broken pipe to notice
# its reader is gone).
tail -n 0 -f "$log" &
tail_pid=$!
if [ "$web" -eq 1 ]; then
  tail -n 0 -f "$web_log" &
  web_tail_pid=$!
fi

# Backgrounded and `wait`-ed rather than run as a plain foreground command:
# bash only checks for a pending trap when a `wait` builtin is interrupted,
# not while blocked on a synchronous foreground child, so a plain foreground
# run would leave Ctrl-C unable to fire `cleanup` until the client itself
# exited -- which, being a reconnect-forever daemon, it never does.
./target/debug/pq-meter-client \
  --endhost-api "$endhost_api" \
  --server "$server_addr" \
  --dummy-meter &
client_pid=$!
wait "$client_pid"
