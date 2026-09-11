# pq-meter-server web API — plan for the backend agent

Plain-HTTP data endpoint for the Angular dashboard in `web/`. This file is the
specification: context, flow, endpoint contract, and verification.

## Context: what exists today

- `src/api.rs` serves an **HTTP/3** application on the SCION socket:
  - `CONNECT` opens a data tunnel; the server pulls meter data from the
    gateway every `PULL_INTERVAL` (5 s). `record_data` appends each received
    measurement to an in-memory `Vec<Value>` and rewrites `data.json`.
  - Everything else goes to a plain `axum::Router` (currently only
    `POST /edh/v1/hello`, `api::DEFAULT_PATH`).
- There is **no plain-TCP HTTP surface** on the server. The plain ports
  31000–31021 are all PocketSCION endhost/SNAP interfaces (`src/network.rs`).
- The Angular dev proxy (`web/proxy.conf.json`) already forwards
  `/edh/v1/*` to **`http://127.0.0.1:31030`**. This API must therefore listen
  on `127.0.0.1:31030`.

## Task

1. **New port constant** in `src/network.rs`, next to the existing port
   constants:
   `pub const WEB_API_PORT: u16 = 31030;`
   Doc comment: plain-HTTP dashboard API (not a PocketSCION interface).
2. **Tokio `net` feature**: the workspace tokio does not enable it. Add
   `features = ["net"]` to the `tokio` dependency in
   `crates/pq-meter-server/Cargo.toml` (needed for
   `tokio::net::TcpListener` / `axum::serve`).
3. **Shared state** between the tunnel and the web API:
   - `Arc<tokio::sync::RwLock<Option<Value>>>` holding the *latest*
     measurement (a JSON object, same shape as one `data.json` entry).
   - Create it in `src/main.rs`; pass clones into `api::serve` and into the
     new web-API task.
   - In `api.rs`, after `record_data` records a batch, write the **last**
     measurement of the batch into the shared state.
4. **Web API task** in `src/main.rs`, spawned before `api::serve`:
   - axum `Router` with one route: `GET /edh/v1/harmonics` (path convention
     follows `api::DEFAULT_PATH`).
   - `axum::serve(TcpListener::bind("127.0.0.1:31030").await?, router)`.
   - Print a startup line like the existing ones, e.g.
     `web API:  http://127.0.0.1:31030/edh/v1/harmonics`.
5. **Handler** for `GET /edh/v1/harmonics`:
   - Read the shared state (read lock).
   - `None` → `503` with body `{"error": "no data yet"}`.
   - `Some(measurement)` → derive the coefficients (below) and answer `200`
     with the JSON below.

## Endpoint specification

`GET /edh/v1/harmonics`

`200 OK` (a measurement has been received):

```json
{
  "frequency_hz": 50,
  "harmonic_count": 25,
  "window_ms": 40,
  "voltage": {
    "a": [325.27, 0.0, 26.02, 0.0, 16.26, 0.0, 9.76, 0.0, 0.0, 0.0, ...],
    "phi_deg": [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, ...]
  },
  "current": {
    "b": [7.07, 0.0, 0.71, 0.0, 0.42, 0.0, 0.21, 0.0, 0.0, 0.0, ...],
    "gamma_deg": [3.0, 3.0, 3.0, 3.0, 3.0, 3.0, 3.0, 3.0, 3.0, 3.0, ...]
  }
}
```

Field semantics (index `i-1` = harmonic `i`, i = 1..25):

| field             | meaning                                          |
| ----------------- | ------------------------------------------------ |
| `frequency_hz`    | fundamental frequency `f` (50)                   |
| `harmonic_count`  | number of harmonics (25)                         |
| `window_ms`       | window the dashboard plots (40 ms = 2 cycles)    |
| `voltage.a[i-1]`  | peak voltage (V) of harmonic `i`                 |
| `voltage.phi_deg[i-1]` | phase φᵢ (deg) of voltage harmonic `i`      |
| `current.b[i-1]`  | peak current (A) of harmonic `i`                 |
| `current.gamma_deg[i-1]` | phase γᵢ (deg) of current harmonic `i`    |

`503 Service Unavailable` (no measurement received yet):
`{"error": "no data yet"}`

The frontend synthesizes the waveforms as
`U_ideal(t) = a₁·sin(2πft)`, `U_real(t) = Σᵢ aᵢ·sin(2πi·ft + φᵢ)`,
`I_ideal(t) = b₁·sin(2πft + γ₁)`, `I_real(t) = Σᵢ bᵢ·sin(2πi·ft + γᵢ)`.

## Derivation (demo synthesis)

The meter only sends RMS summaries (`voltage_l1_v`, `current_l1_a`,
`phase_angle_l1_deg`, …), so the coefficients are **derived**, not measured.
Keep the derivation in one small, well-named function so it can be replaced
later — the UMG 605 Pro can deliver a real harmonic spectrum over Modbus.

From the latest measurement (example: dummy meter ≈ 230 V / 5 A / 3°):

- `a[0]  = sqrt(2) * voltage_l1_v`          (≈ 325.27 V peak)
- `a[2]  = 0.08 * a[0]`                      (3rd harmonic)
- `a[4]  = 0.05 * a[0]`                      (5th)
- `a[6]  = 0.03 * a[0]`                      (7th)
- all other `a[i] = 0`
- `phi_deg[i] = 0` for all `i` (fundamental voltage is the phase reference)
- `b[0]  = sqrt(2) * current_l1_a`           (≈ 7.07 A peak)
- `b[2]  = 0.10 * b[0]`
- `b[4]  = 0.06 * b[0]`
- `b[6]  = 0.03 * b[0]`
- all other `b[i] = 0`
- `gamma_deg[i] = phase_angle_l1_deg` for all `i`

## Verification

- `cargo build` and `cargo test` pass for the workspace.
- Start server and client (`scripts/run-dummy.sh`), then:
  - `curl -s -o /dev/null -w "%{http_code}" http://127.0.0.1:31030/edh/v1/harmonics`
    → `503` before the first pull, `200` after (first pull is immediate).
  - The response body parses as JSON and matches the table above.
- No regression: the CONNECT tunnel still works and `data.json` is still
  rewritten as before.

## Constraints / out of scope

- Loopback-only bind, no TLS (development use).
- No CORS — the dashboard talks to the API through the dev proxy
  (same-origin) until a real deployment exists.
- Do not change the CONNECT tunnel protocol or the HTTP/3 surface.
- No new dependencies beyond enabling tokio's `net` feature (axum, tokio,
  serde_json are already dependencies of this crate).
