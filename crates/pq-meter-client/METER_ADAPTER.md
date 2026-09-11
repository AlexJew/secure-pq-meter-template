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
- **The meter itself can't answer "give me everything since index N"** — a `read_snapshot`
  always returns exactly one fresh reading, taken now. Backfill for a reconnecting/catching-up
  peer is instead served from the client's own persisted history (see "Wiring into the client"
  below), not by re-querying the meter.

`MeterSnapshot::to_json(self, index)` renders one snapshot in the wire shape, stamping it with
a caller-supplied index and the current time. It's exercised directly by a `meter.rs` test
(see "Tests" below) to pin down the wire field names, but the live reply path does not call it:
once a snapshot is persisted, the reply is built from stored rows by `stored_reading_json` in
`main.rs` (see below), whose `index` is the SQLite row id rather than a value `to_json` stamps
itself.

## `DummyMeter`

A `MeterSource` for demos and tests, alongside the real `ModbusMeter` (see below). Each call to
`read_snapshot` returns plausible values (230V, 5A, ~1150W, etc.) perturbed by a deterministic
sine-based jitter keyed on a read counter, so consecutive readings visibly differ without
needing real entropy or state beyond a `u64`.

## Wiring into the client

`crates/pq-meter-client/src/main.rs` groups the state that must survive a tunnel reconnect —
the meter and the local database every reading is appended to — into one struct:

```rust
struct Gateway {
    meter: Box<dyn MeterSource>,
    store: MeterStore,
}
```

`run` (the CONNECT-tunnel entry point) constructs one `Gateway` before entering its reconnect
loop, so the meter's state and the readings persisted so far keep going across reconnects
instead of resetting. There's no separate wire-index counter to keep in sync across
reconnects — see below for where the index actually comes from.

The request-handling code is split so the protocol logic is independent of the transport:

- `run_tunnel` — opens the HTTP/3 `CONNECT` request, checks the response status, and builds the
  `H3DuplexStream`. This is the only function that talks to the network.
- `serve_tunnel<S: AsyncRead + AsyncWrite + Unpin>` — the NDJSON framing loop: read bytes,
  split on `\n`, enforce the max line size, dispatch each line. Generic over the stream type, so
  it runs identically over the real tunnel or an in-memory `tokio::io::duplex()` pipe.
- `handle_line` — parses one `"data"` request and calls `gateway.meter.read_snapshot()` for one
  fresh reading, then persists it to `gateway.store` (a storage failure is logged, not surfaced
  on the wire — persistence is a side channel, not part of the protocol). The reply's
  `payload.data` is then *every* stored row after the request's `payload.index`, read back via
  `gateway.store.since_id` and rendered by `stored_reading_json` (see below) — so a request can
  come back with more than one reading if the peer is catching up, and the wire `index` is each
  row's SQLite id, not a counter the client tracks separately. On a meter failure, `handle_line`
  replies `"error"` with the meter's error message. An unrecognized `"type"` also gets an
  `"error"` reply without closing the tunnel.

## Local persistence (`storage.rs`) and `show`

Every reading the tunnel answers is also appended to a SQLite file via `MeterStore`
(`crates/pq-meter-client/src/storage.rs`) — its own module, independent of both `meter.rs` and
the tunnel logic. It stores rows keyed by an auto-incrementing id and a millisecond timestamp,
in WAL mode so a concurrent reader (like `show`, below) doesn't block the writer.

`MeterSnapshot` and `storage::Reading` are deliberately two separate types (`meter.rs` doesn't
know about SQLite; `storage.rs` doesn't know about the wire protocol) bridged by one `impl
From<MeterSnapshot> for Reading` in `main.rs`. The reverse direction — a stored row back to wire
JSON — is `stored_reading_json` in `main.rs`, used both by the tunnel's `"data"` replies and by
`show --json`, so the two never drift apart.

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
  with a `voltage_l1_v` field and is also persisted to the store; a request whose `payload.index`
  is behind the store gets every row after it, oldest first (the backfill-from-history behavior
  described above); the wire index increases across requests; an unknown `"type"` gets an
  `"error"` reply without closing the tunnel; a meter that always fails (`BrokenMeter`, defined
  only in the test module) produces an `"error"` reply instead of killing the loop; invalid JSON
  and an over-long line without a delimiter both close the tunnel. A few more check the CLI
  parsing itself: bare invocation vs. `show`, `select_rows`'s `--last`/`--since-id` behavior, and
  that `--meter-ip`/`--dummy-meter` are mutually exclusive but exactly one is required.

## Running it: `scripts/run-dummy.sh`

```bash
scripts/run-dummy.sh          # hermetic: builds, then runs the tests above, printing
                               # one real "data" reply the client produced. No server needed.
scripts/run-dummy.sh --live   # starts pq-meter-server, scrapes its (randomly assigned)
                               # SCION address from its output, and points the client at it.
```

`--live` builds and execs the actual binaries (not `cargo run`, whose `$!` would be cargo's own
PID rather than the server's), scrapes the server's SCION address by polling a redirected log
file, and cleans up the background server on exit or `Ctrl-C`. It runs the client with
`--dummy-meter` so it has no real hardware to reach. `pq-meter-server` implements the `CONNECT`
tunnel endpoint from `CONNECT_PROTOCOL.md`, so this mode opens a real tunnel and you should see
the server pulling and printing dummy measurements from the client every few seconds.

## The real meter (`ModbusMeter`) and `--dummy-meter`

`main` wires in [`meter::ModbusMeter`], which connects to a real UMG 605-PRO over Modbus TCP at
startup using `--meter-ip`/`--meter-port`/`--meter-unit`/`--meter-timeout-secs`. Passing
`--dummy-meter` instead skips that connection and wires in [`meter::DummyMeter`] — plausible,
drifting values with no hardware required, for demos and `scripts/run-dummy.sh --live`.
`--meter-ip` and `--dummy-meter` are mutually exclusive, and clap requires exactly one of them
to be given.
