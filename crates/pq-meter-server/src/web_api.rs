//! Plain-HTTP data API for the Angular dashboard in `web/`.
//!
//! Unlike `api.rs`'s HTTP/3 endpoint — reachable only through the simulated
//! SCION network — this serves plain HTTP on loopback: the dashboard's dev
//! proxy (`web/proxy.conf.json`) forwards `/edh/v1/*` straight to it, no
//! SCION stack involved. It shares its state with the tunnel in `api.rs`
//! (the same `Arc`s created in `main.rs`), so a reading pulled from a
//! gateway is visible here as soon as it's committed:
//!
//! * `GET /edh/v1/readings` queries [`ServerStore`] directly — see
//!   [`ServerStore::readings`].
//! * `GET /edh/v1/harmonics` reads the *latest* raw measurement (kept
//!   in-memory by `api::record_data`, not queried from `store`) and derives
//!   a harmonic spectrum from it for the dashboard's waveform view — see
//!   `API.md` for the endpoint's full contract and [`derive_harmonics`] for
//!   how the response is built.

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, Mutex, RwLock},
};

use axum::{
    Json, Router,
    extract::{Query, State},
    http::StatusCode,
    routing::get,
};
use serde_json::{Value, json};
use tokio::net::TcpListener;

use crate::storage::ServerStore;

/// Path the dashboard fetches recent readings from.
pub const READINGS_PATH: &str = "/edh/v1/readings";
/// Path the dashboard fetches the derived harmonic spectrum from.
pub const HARMONICS_PATH: &str = "/edh/v1/harmonics";

/// Most rows a single `GET` on [`READINGS_PATH`] answers, regardless of the
/// requested `limit`.
const MAX_LIMIT: usize = 2000;
/// Rows returned when the caller does not pass `limit`.
const DEFAULT_LIMIT: usize = 200;

/// Harmonics reported in a [`HARMONICS_PATH`] response — matches the count
/// both `MeterSource` implementations report (see
/// `pq-meter-client/src/meter/{dummy,umg605}.rs`'s `HARMONIC_COUNT`), but is
/// this endpoint's own constant rather than shared with them: it describes
/// the response shape, not the gateway's capability.
const HARMONIC_COUNT: usize = 25;
/// The waveform window the dashboard plots: two cycles at [`FREQUENCY_HZ`].
const WINDOW_MS: f64 = 40.0;
/// Fixed mains frequency — no reading names one, and 50 Hz is what this
/// project's meters (and demo) run at.
const FREQUENCY_HZ: f64 = 50.0;

/// State shared by every route below.
#[derive(Clone)]
struct AppState {
    store: Arc<Mutex<ServerStore>>,
    /// The most recent raw measurement object received over the tunnel, or
    /// `None` before the first one — see `api::record_data`.
    latest: Arc<RwLock<Option<Value>>>,
}

/// Serves the dashboard's plain-HTTP data API on `bind_addr` until the
/// process stops.
pub async fn serve(
    bind_addr: SocketAddr,
    store: Arc<Mutex<ServerStore>>,
    latest: Arc<RwLock<Option<Value>>>,
) -> anyhow::Result<()> {
    let app = Router::new()
        .route(READINGS_PATH, get(readings))
        .route(HARMONICS_PATH, get(harmonics))
        .with_state(AppState { store, latest });
    let listener = TcpListener::bind(bind_addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

/// `GET /edh/v1/readings[?gateway=<id>][&since_id=<id>][&limit=<n>]`
///
/// Answers up to `limit` readings (default [`DEFAULT_LIMIT`], capped at
/// [`MAX_LIMIT`]) with `id` greater than `since_id` (default `0`, i.e.
/// everything), oldest first — see [`ServerStore::readings`]. Pass `gateway`
/// to see only one gateway's rows; omit it to see every gateway's, which is
/// fine for a single-gateway demo.
async fn readings(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<Vec<Value>>, (StatusCode, Json<Value>)> {
    let since_id: i64 = parse_param(&params, "since_id")?.unwrap_or(0);
    let limit: usize = parse_param(&params, "limit")?.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT);
    let gateway = params.get("gateway").map(String::as_str);

    state
        .store
        .lock()
        .unwrap()
        .readings(gateway, since_id, limit)
        .map(Json)
        .map_err(|err| {
            (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": err.to_string() })))
        })
}

/// `GET /edh/v1/harmonics` — see `API.md` for the full contract. `503` with
/// `{"error": "no data yet"}` before the first measurement has arrived,
/// otherwise `200` with [`derive_harmonics`]'s output for the latest one.
async fn harmonics(State(state): State<AppState>) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    match state.latest.read().unwrap().clone() {
        Some(measurement) => Ok(Json(derive_harmonics(&measurement))),
        None => Err((StatusCode::SERVICE_UNAVAILABLE, Json(json!({ "error": "no data yet" })))),
    }
}

/// Builds a [`HARMONICS_PATH`] response from one raw measurement.
///
/// Both `MeterSource` implementations already report a real per-harmonic
/// spectrum — fields named `{voltage,current}_harmonic_{mag,phase}_l1_h
/// {order}_{v,a,deg}` for `order` 1..=[`HARMONIC_COUNT`], see
/// `pq-meter-client/src/meter/mod.rs`'s `push_harmonic_array` — so those are
/// read directly rather than approximated from the RMS summary the way
/// `API.md`'s original derivation sketch (written before that per-harmonic
/// data existed on the wire) describes; that sketch's own note that this
/// should "be replaced later" once the meter can deliver a real spectrum
/// applies now. A harmonic order missing from `measurement` (an older
/// gateway, say) reads back `0.0` rather than failing the whole response.
fn derive_harmonics(measurement: &Value) -> Value {
    let harmonic_array = |base: &str, unit: &str| -> Vec<f64> {
        (1..=HARMONIC_COUNT)
            .map(|order| {
                measurement
                    .get(format!("{base}_h{order}_{unit}"))
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0)
            })
            .collect()
    };

    json!({
        "frequency_hz": FREQUENCY_HZ,
        "harmonic_count": HARMONIC_COUNT,
        "window_ms": WINDOW_MS,
        "voltage": {
            "a": harmonic_array("voltage_harmonic_mag_l1", "v"),
            "phi_deg": harmonic_array("voltage_harmonic_phase_l1", "deg"),
        },
        "current": {
            "b": harmonic_array("current_harmonic_mag_l1", "a"),
            "gamma_deg": harmonic_array("current_harmonic_phase_l1", "deg"),
        },
    })
}

/// Parses the query parameter `name` out of `params`, if present.
fn parse_param<T: std::str::FromStr>(
    params: &HashMap<String, String>,
    name: &str,
) -> Result<Option<T>, (StatusCode, Json<Value>)> {
    params
        .get(name)
        .map(|value| value.parse())
        .transpose()
        .map_err(|_| {
            (StatusCode::BAD_REQUEST, Json(json!({ "error": format!("{name} must be an integer") })))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_harmonics_reads_the_per_harmonic_fields() {
        let mut measurement = serde_json::Map::new();
        measurement.insert("voltage_harmonic_mag_l1_h1_v".to_string(), json!(325.27));
        measurement.insert("voltage_harmonic_phase_l1_h1_deg".to_string(), json!(0.0));
        measurement.insert("current_harmonic_mag_l1_h1_a".to_string(), json!(7.07));
        measurement.insert("current_harmonic_phase_l1_h1_deg".to_string(), json!(3.0));
        measurement.insert("voltage_harmonic_mag_l1_h3_v".to_string(), json!(26.02));

        let response = derive_harmonics(&Value::Object(measurement));

        assert_eq!(response["frequency_hz"], json!(50.0));
        assert_eq!(response["harmonic_count"], json!(25));
        assert_eq!(response["voltage"]["a"][0], json!(325.27));
        assert_eq!(response["voltage"]["a"][2], json!(26.02));
        assert_eq!(response["current"]["b"][0], json!(7.07));
        assert_eq!(response["current"]["gamma_deg"][0], json!(3.0));
        assert_eq!(response["voltage"]["a"].as_array().unwrap().len(), HARMONIC_COUNT);
    }

    #[test]
    fn derive_harmonics_defaults_missing_harmonics_to_zero() {
        let response = derive_harmonics(&json!({}));

        for series in [
            &response["voltage"]["a"],
            &response["voltage"]["phi_deg"],
            &response["current"]["b"],
            &response["current"]["gamma_deg"],
        ] {
            assert!(series.as_array().unwrap().iter().all(|v| *v == json!(0.0)));
        }
    }
}
