# Meter data adapter

## Purpose

`pq-meter-client` needs to read a meter every time the server sends a `"data"`
request (see `CONNECT_PROTOCOL.md`). This document describes the seam that
separates "how a request is answered" from "where the numbers come from", and
the dummy implementation behind it that lets the client be built, run, and
tested without a UMG 605-PRO attached.

## The `MeterSource` trait

Defined in `crates/pq-meter-client/src/meter.rs`:

```rust
pub struct MeterSnapshot {
    pub voltage_l1_v: f64,
    pub current_l1_a: f64,
    pub active_power_l1_w: f64,
    pub reactive_power_l1_var: f64,
    pub phase_angle_l1_deg: f64,
}

#[async_trait::async_trait]
pub trait MeterSource: Send {
    async fn read_snapshot(&mut self) -> anyhow::Result<MeterSnapshot>;
}
```

Design points:

- **`&mut self`.** The real UMG 605-PRO client (`umg605_modbus_client::Umg605ProClient`)
  reads over a Modbus TCP connection, and every one of its read methods (`voltage_l1`,
  `current_l1`, `power_l1_n`, `reactive_power_l1`, `phase_angle_l1`) takes `&mut self`. Matching
  that here means a real adapter needs no `Mutex` just to satisfy the trait.
- **`#[async_trait]`, not native `async fn` in trait.** Native async-in-trait is stable but not
  dyn-compatible. The gateway holds the meter as `Box<dyn MeterSource>` so that swapping the
  dummy for the real thing is a one-line change in `main`, not a restructuring — that only
  works with `async_trait`'s boxed-future desugaring.
- **`anyhow::Result`, not a bespoke error enum.** The only thing that happens to a
  `read_snapshot` error is that it gets formatted into the protocol's `"error"` reply
  (`CONNECT_PROTOCOL.md`'s "Error Reply"). Nothing ever matches on a specific failure variant, so
  a typed enum would add ceremony without adding safety. The real adapter's `ReadError`
  (a `thiserror` enum) converts automatically via `?`.
- **No `index` or `timestamp` fields on `MeterSnapshot`.** Those are wire/protocol concerns
  owned by the client's request-handling loop (see below), not readings a meter produces.
- **The backfill range in `payload.index` is not implemented.** Every `"data"` request gets
  answered with exactly one fresh snapshot, taken now. Real hardware has no way to answer "give
  me everything since index N" anyway.

`MeterSnapshot::to_json(self, index)` renders one entry of a `"data"` reply's
`payload.data` array, stamping it with the wire index (owned by the caller) and
the current time:

```json
{
  "timestamp": "1789060575132",
  "voltage_l1_v": 231.29,
  "current_l1_a": 5.32,
  "active_power_l1_w": 1182.21,
  "reactive_power_l1_var": 66.44,
  "phase_angle_l1_deg": 3.64,
  "index": 1
}
```

## `DummyMeter`

The only implementation of `MeterSource` today. Each call to `read_snapshot` returns
plausible values (230V, 5A, ~1150W, etc.) perturbed by a deterministic sine-based jitter keyed
on a read counter, so consecutive readings visibly differ without needing real entropy or state
beyond a `u64`.

## Wiring into the client

`crates/pq-meter-client/src/main.rs` groups the state that must survive a tunnel reconnect —
the meter, the local database every reading is appended to, and the running wire index — into
one struct:

```rust
struct Gateway {
    meter: Box<dyn MeterSource>,
    store: MeterStore,
    next_index: u64,
}
```

`run` (the CONNECT-tunnel entry point) constructs one `Gateway` before entering its reconnect
loop, so the meter's state, the readings persisted so far, and the wire index all keep going
across reconnects instead of resetting.

The request-handling code is split so the protocol logic is independent of the transport:

- `run_tunnel` — opens the HTTP/3 `CONNECT` request, checks the response status, and builds the
  `H3DuplexStream`. This is the only function that talks to the network.
- `serve_tunnel<S: AsyncRead + AsyncWrite + Unpin>` — the NDJSON framing loop: read bytes,
  split on `\n`, enforce the max line size, dispatch each line. Generic over the stream type, so
  it runs identically over the real tunnel or an in-memory `tokio::io::duplex()` pipe.
- `handle_line` — parses one request and calls `gateway.meter.read_snapshot()`. On success, it
  appends the snapshot to `gateway.store` (a storage failure is logged, not surfaced on the
  wire — persistence is a side channel, not part of the protocol), stamps the snapshot with the
  next wire index, and replies `"data"`; on a meter failure, it replies `"error"` with the
  meter's error message. An unrecognized `"type"` also gets an `"error"` reply without closing
  the tunnel.

## Local persistence (`storage.rs`) and `show`

Every reading the tunnel answers is also appended to a SQLite file via `MeterStore`
(`crates/pq-meter-client/src/storage.rs`) — its own module, independent of both `meter.rs` and
the tunnel logic. It stores rows keyed by an auto-incrementing id and a millisecond timestamp,
in WAL mode so a concurrent reader (like `show`, below) doesn't block the writer.

`MeterSnapshot` and `storage::Reading` are deliberately two separate types (`meter.rs` doesn't
know about SQLite; `storage.rs` doesn't know about the wire protocol) bridged by one `impl
From<MeterSnapshot> for Reading` in `main.rs` — the only place that needs to know about both.

`pq-meter-client show` reads that file back without touching the network:

```bash
pq-meter-client show                    # the 20 most recent readings, newest first, as a table
pq-meter-client show --since-id 42      # everything after row 42, oldest first
pq-meter-client show --json             # one JSON object per line instead of a table
```

The CLI is one `clap` parser with an optional subcommand: with no subcommand it parses
`--endhost-api`/`--server`/`--db` and runs the tunnel; `show` parses its own flags and does not
need the SCION addresses (`args_conflicts_with_subcommands` / `subcommand_negates_reqs` enforce
that the two don't mix).

## Tests

`cargo test -p pq-meter-client` runs entirely without a network or a meter:

- `meter.rs` tests check that `DummyMeter` produces varying, plausible values and that
  `to_json` carries the right wire fields.
- `storage.rs` tests check `MeterStore` directly: insert/query round-trips, ordering, the
  `since_id` cursor, and that a file-backed store survives being reopened.
- `main.rs` tests drive `serve_tunnel` over `tokio::io::duplex()` with an in-memory
  (`":memory:"`) `MeterStore`, asserting: a `"data"` request gets a matching-id `"data"` reply
  with a `voltage_l1_v` field and is also persisted to the store; the wire index increases
  across requests; an unknown `"type"` gets an `"error"` reply without closing the tunnel; a
  meter that always fails (`BrokenMeter`, defined only in the test module) produces an
  `"error"` reply instead of killing the loop; invalid JSON and an over-long line without a
  delimiter both close the tunnel. A few more check the CLI parsing itself (bare invocation vs.
  `show`, and `select_rows`'s `--last`/`--since-id` behavior).

## Running it: `scripts/run-dummy.sh`

```bash
scripts/run-dummy.sh          # hermetic: builds, then runs the tests above, printing
                               # one real "data" reply the client produced. No server needed.
scripts/run-dummy.sh --live   # starts pq-meter-server, scrapes its (randomly assigned)
                               # SCION address from its output, and points the client at it.
```

`--live` builds and execs the actual binaries (not `cargo run`, whose `$!` would be cargo's own
PID rather than the server's), scrapes the server's SCION address by polling a redirected log
file, and cleans up the background server on exit or `Ctrl-C`. Since `pq-meter-server` does not
yet implement the `CONNECT` tunnel endpoint from `CONNECT_PROTOCOL.md`, this mode currently shows
the client logging a `404 Not Found` and retrying with backoff — that is the expected result
until the server side lands, not a broken setup.

## Adding the real meter later

1. Add `umg605-modbus-client` back to `crates/pq-meter-client/Cargo.toml`.
2. In `meter.rs`, add a type (e.g. `Umg605Meter`) holding a connected `Umg605ProClient`, and
   implement `MeterSource` for it: `read_snapshot` awaits `voltage_l1()`, `current_l1()`,
   `power_l1_n()`, `reactive_power_l1()` and `phase_angle_l1()` in turn (each an `f32`, convert
   with `f64::from`), propagating any `ReadError` with `?` — it converts to `anyhow::Error` for
   free.
3. In `main`, construct that type instead of `DummyMeter` (behind a CLI flag if both are meant
   to coexist). Nothing in `run_tunnel`, `serve_tunnel`, or `handle_line` needs to change.
