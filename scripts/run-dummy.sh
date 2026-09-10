#!/usr/bin/env bash
# Runs pq-meter-client against its dummy meter (see crates/pq-meter-client/src/meter.rs).
#
#   scripts/run-dummy.sh          hermetic: build and run the client's tests,
#                                 printing a real NDJSON reply. No network needed.
#   scripts/run-dummy.sh --live   start pq-meter-server, scrape its (randomly
#                                 assigned) SCION address from its output, and
#                                 point the client at it.
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo_root"

mode=${1:-}

case "$mode" in
"")
  echo "==> building pq-meter-client"
  cargo build -p pq-meter-client

  echo
  echo "==> running the client's tests (the 'dummy data reply: ...' line below is"
  echo "    a real reply the client produced over an in-memory tunnel, no server needed)"
  echo
  cargo test -p pq-meter-client -- --nocapture
  ;;

--live)
  echo "==> building pq-meter-server and pq-meter-client"
  cargo build -p pq-meter-server -p pq-meter-client

  log=$(mktemp "${TMPDIR:-/tmp}/pq-meter-server.XXXXXX")
  server_pid=""
  client_pid=""

  cleanup() {
    for pid in "$client_pid" "$server_pid"; do
      if [ -n "$pid" ]; then
        kill "$pid" >/dev/null 2>&1 || true
        wait "$pid" 2>/dev/null || true
      fi
    done
    rm -f "$log"
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
  echo "NOTE: pq-meter-server does not implement the CONNECT tunnel endpoint yet"
  echo "      (see CONNECT_PROTOCOL.md), so the client below is expected to log"
  echo "      a 404 and back off/retry -- that is not a broken setup."
  echo "      Press Ctrl-C to stop; the server is shut down with it."
  echo

  # Backgrounded and `wait`-ed rather than run as a plain foreground command:
  # bash only checks for a pending trap when a `wait` builtin is interrupted,
  # not while blocked on a synchronous foreground child, so a plain foreground
  # run would leave Ctrl-C unable to fire `cleanup` until the client itself
  # exited -- which, being a reconnect-forever daemon, it never does.
  ./target/debug/pq-meter-client \
    --endhost-api "$endhost_api" \
    --server "$server_addr" &
  client_pid=$!
  wait "$client_pid"
  ;;

*)
  echo "usage: $0 [--live]" >&2
  exit 2
  ;;
esac
