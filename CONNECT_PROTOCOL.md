# PQ Meter CONNECT Protocol

## Purpose

`pq-meter-client` maintains a persistent, bidirectional connection to
`pq-meter-server`. The server requests meter data and the client reads the
UMG 605-PRO before returning the requested data.

The protocol uses an HTTP/3 `CONNECT` tunnel over QUIC and SCION. It is not a
WebSocket connection.

## Transport Layers

```text
JSON protocol messages
        |
HTTP/3 CONNECT request and response bodies
        |
QUIC bidirectional stream
        |
SCION packets
```

The client opens an HTTP/3 request with method `CONNECT`. HTTP/3 maps that
request to a QUIC bidirectional stream:

- The HTTP request body flows from client to server.
- The HTTP response body flows from server to client.
- The two directions are independent and can carry data concurrently.

After the server returns a successful `2xx` response, both peers keep their
half of the stream open. Neither side sends a final HTTP body nor shuts down
its write half while the tunnel is active.

## Connection Flow

```text
pq-meter-client                              pq-meter-server
       |                                            |
       | -- CONNECT, x-pq-gateway-id: pi-north ---> |
       |                                            |
       | <--------------------- 200 OK ------------ |
       |                                            |
       | <--- {"type":"data","id":1,"payload":{}}\n
       |                                            |
       | --- {"type":"data","id":1,"payload":{"data":{...},"latest_index":1}}\n --> |
       |                                            |
```

The client always initiates the tunnel. The server issues data requests only
after it has accepted the client connection and retained the server-to-client
write handle for that tunnel.

## Gateway Identity and History Resets

The `CONNECT` request carries one header, `x-pq-gateway-id`, naming the
gateway opening the tunnel (`pq-meter-client`'s `--gateway-id`, defaulting to
its endhost API's `host:port`). This is a header on the request the client
was already sending, not a second connection: the pinned SCION SDK forwards
ordinary headers on a `CONNECT` request in both directions, even though it
drops `:path`/`:scheme` for it (classic RFC 9114 `CONNECT`).

The server persists received history and its pull cursor keyed on this
identity, so multiple gateways sharing one server don't collide, and a
reconnecting gateway resumes from its own last-pulled index instead of
re-pulling (and double-counting) its entire history from `0`. A tunnel with
no header — an older client — is attributed to the identity `"unknown"` and a
warning is logged; a second such gateway would mix into the same history.

The client's local database (`pqmeter.db`) is a transient replay buffer (see
`DESIGN_DECISIONS.md`) that may be deleted and recreated at any time, which
resets the client's own row ids back down near `0`. Without something to
notice this, the server would keep asking "everything after index N" for an
N the fresh database will never reach again, and no data would ever flow.
Every Data Reply therefore also carries `payload.latest_index`: the client's
current high-water mark, reported whether or not that reply's `data` array is
empty. If a reply's `latest_index` is lower than the index the server just
asked "everything after", the server resets that gateway's cursor to `0`, so
its next request asks for everything and the gateway's data resumes flowing
within the next pull. (A client too old to send `latest_index` is never
reset this way — a missing field is not treated as a drop to zero.)

This detects a shrunk history by it going backwards, so it cannot detect one
that has already climbed back past the old mark before the server next talks
to that gateway — for example, the client run alone in `record` mode for long
enough after its database was recreated. That gap is accepted: closing it
would need an identity tied to the database file itself (e.g. a stored
creation-time epoch) rather than a number that only moves forward.

## Message Framing

Messages use newline-delimited JSON (NDJSON). Each JSON object is encoded as
UTF-8 and terminated by one newline byte (`\n`).

```text
{"type":"data","id":1,"payload":{}}\n
{"type":"data","id":2,"payload":{}}\n
```

HTTP/3 `DATA` frames and reads from `AsyncRead` are byte chunks, not protocol
messages. One JSON message can arrive in multiple chunks, or a chunk can
contain several messages. Each peer must therefore:

1. Append received bytes to a per-tunnel buffer.
2. Extract every complete line ending in `\n`.
3. Parse each extracted line as one JSON object.
4. Retain the unfinished final line in the buffer for the next read.

Set a maximum line size and close the tunnel when it is exceeded or when a
line is invalid JSON. This prevents an unbounded buffer if a peer never sends
a delimiter.

## Message Schema

Every message has these fields:

```json
{
  "type": "data",
  "id": 1,
  "payload": {}
}
```

- `type`: command or reply kind. The first supported value is `"data"`.
- `id`: request identifier selected by the server. It must be preserved by the
  client in its reply and is unique among outstanding requests on a tunnel.
- `payload`: message-specific JSON object.

### Data Request

The server sends this message on the HTTP response body:

```json
{
    "type": "data",
    "id": 42,
    "payload": {
        "index": 0 // 0 means from beginning of time, n means from n index until now
    } // Payload on command empty
}
```

The empty payload means “read the default meter snapshot”. Future versions may
add an explicit list of measurements or options to this object.

### Data Reply

The client sends this message on the HTTP request body after reading the meter:

```json
{
    "type": "data",
    "id": 42, // Same ID as request
    "payload": {
        "data": [ // Batched data
            {
                "timestamp": "standard formatted unixtimestamp",
                "value1": "Any data",
                "value2": "Any data",
                "index": 1 // Increasing index after each data request
            },
            {
                "timestamp": "standard formatted unixtimestamp",
                "value1": "Any data",
                "value2": "Any data",
                "index": 2 // Increasing index after each data request
            },
        ],
        "latest_index": 2 // The client's current high-water mark; see
                          // "Gateway Identity and History Resets" above.
                          // Present even when "data" is empty.
    }
}
```

Numeric field names include units. The exact snapshot should be implemented
from the existing methods in `umg605-modbus-client`.

### Error Reply

If the client cannot understand a request or cannot read the meter, it must
reply with the same `id` rather than silently dropping the request:

```json
{
  "type": "error",
  "id": 1,
  "payload": {
    "message": "unable to read voltage_l1: connection timed out"
  }
}
```

The server treats an `"error"` reply as completion of that request.

## Implementation Plan

### Server

1. Change the current POST route in `pq-meter-server` to accept `CONNECT` on
   a dedicated tunnel path, such as `/edh/v1/tunnel`.
2. Return `200 OK` with a streaming Axum response body. The body must stay
   open and is the server-to-client direction.
3. Retain a channel sender for each live client tunnel. Writing an NDJSON data
   request to that channel sends it to the client.
4. Read the Axum request body in a background task, frame NDJSON replies, and
   resolve each reply against its `id`.
5. Remove the tunnel and fail its outstanding requests when either direction
   closes or fails.

`scion-h3-axum` passes streaming request and response bodies through to Axum,
so it can serve this endpoint. It does not use Axum's `WebSocketUpgrade`.

### Client

1. Replace the one-shot high-level `scion_http3::Client` POST with the lower
   level `scion_quic::h3::client::Http3Client`.
2. Create an HTTP `CONNECT` request and call `request_with_writer`.
3. Await the response headers and require a successful status.
4. Build `H3DuplexStream` from the returned request writer and response body.
   It implements `AsyncRead` and `AsyncWrite` over the two tunnel directions.
5. Read and frame server commands continuously. For each `"data"` command,
   use `Umg605ProClient` to collect the snapshot, then write an NDJSON reply
   carrying the original `id`.
6. Reconnect with bounded backoff when the tunnel closes unexpectedly.

## Operational Rules

- A client must have at most one active tunnel for a given meter identity.
- The server should limit the number of outstanding requests per client.
- Apply a timeout to each request ID; expire it if no reply arrives.
- Log tunnel open, close, invalid-message, timeout, and meter-read failures.
- The current example uses a self-signed certificate with peer verification
  disabled. Enable certificate verification before deployment outside the
  development environment.