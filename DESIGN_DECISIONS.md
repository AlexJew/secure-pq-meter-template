# Design decisions

Brief, non-obvious decisions and the reasoning behind them, gathered in one
place so they don't get re-litigated or lost in a single crate's doc. Each
entry links to the fuller doc when one exists.

## Storage: client DB is transient, server DB is persistent

`pq-meter-client`'s SQLite file only exists to make forwarding reliable — it's
a replay buffer so a reconnecting gateway can resume from its last acked
index instead of losing readings to a Wi-Fi blip or a Pi reboot. It is not
meant to be queried for analysis, so it's fine to delete and recreate it on a
schema change rather than migrate it.

The server's store is the opposite: it's the durable, queryable copy meant
for analysis, dashboards (see `crates/pq-meter-server/TODO.md`'s "Grafana"
section), and anything that needs history across gateways. This is why the
server has its own `ServerStore` (`crates/pq-meter-server/src/storage.rs`)
rather than treating `data.json` (removed) or the client's DB as the
analysis-ready copy. Unlike the client's store, its `readings` table's value
columns are nullable and only ever grow (`ALTER TABLE ADD COLUMN`, never a
migration or a backfilled default) — see the "fail loud" entry below for why
that's the opposite rule from the client's, deliberately.

## Server history survives a gateway reconnect; gateways need an identity

A gateway (`pq-meter-client`) always asks the server for "everything since
its last acked index" from its own history, and a `CONNECT` tunnel is a fresh
in-memory session on the server, per connection. Naively starting each
session's pull cursor at `0` — as the very first server implementation did —
means a gateway reconnecting after any network hiccup gets its entire history
re-sent and re-counted from the beginning, every time. `ServerStore` persists
the pull cursor per gateway identity precisely so a reconnect resumes instead
of restarting.

That identity has to come from somewhere the client controls: the tunnel's
own `authority` is the server's own TLS name (`pq-meter-server:<port>`,
identical for every client), so it cannot tell gateways apart. The client
instead sends an `x-pq-gateway-id` header on the `CONNECT` request itself —
one extra header on the request that was already being sent, not a second
connection or a change to the NDJSON message schema. See
`CONNECT_PROTOCOL.md`'s "Gateway Identity and History Resets" section for the
full mechanism, including how a client's own database being deleted and
recreated (its row ids restarting near `0`) is detected and self-heals via a
`latest_index` field on every Data Reply, rather than needing yet another
piece of persisted identity.

## Fail loud on schema change, don't auto-migrate

Because the client DB is a transient replay buffer (above), a changed
`readings` schema (a meter reporting different columns, a code change) should
make the client fail and require deleting `pqmeter.db`, not silently
migrate/backfill defaults for missing columns. A backfilled default is a
fabricated reading; a demo losing its buffer is not a real problem.

## Meter adapter: `MeterSource` is vendor-agnostic by construction

See `crates/pq-meter-client/METER_ADAPTER.md` for the full design. The short
version: `MeterSnapshot` is an open, ordered list of named readings rather
than fixed fields, so a second meter vendor is a sibling module implementing
the same trait — nothing in the wire protocol, storage schema, or server
changes. `storage.rs` derives its SQL columns from whatever names the first
snapshot carries, rather than assuming a fixed set.

`kind()` is a label for logs, not a storage or protocol dispatch key — only
one meter runs per gateway process, so there's nothing to dispatch on.

## Transport: HTTP/3 CONNECT tunnel, not a one-shot request or WebSocket

See `CONNECT_PROTOCOL.md` for the full protocol. The server needs to pull
fresh meter data on its own schedule from a client that may reconnect at any
time, so the connection is a long-lived bidirectional tunnel (HTTP/3
`CONNECT` over QUIC/SCION) rather than a one-shot POST per reading — and
plain HTTP/3, not a WebSocket, since `scion-h3-axum` passes streaming
request/response bodies straight through without needing `WebSocketUpgrade`.
Messages are newline-delimited JSON with a maximum line size, so a peer that
never sends a delimiter can't grow the frame buffer unbounded.
