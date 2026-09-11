//! The HTTP/3 endpoint that receives data from the gateway.
//!
//! Two kinds of traffic are served on the same endpoint:
//!
//! * `CONNECT` requests open a bidirectional tunnel. Over it, the server pulls
//!   meter data from the gateway: every few seconds it sends a `data` request
//!   naming the last index it received, and the gateway answers with all
//!   measurements since that index. Each received measurement is printed on
//!   stdout and stored in the server's SQLite database (see [`storage`]).
//! * Everything else is served by a plain [`axum::Router`], so the
//!   `POST /edh/v1/hello` endpoint of the original template keeps working and
//!   you can add more routes the usual way.
//!
//! Messages on the tunnel are newline-delimited JSON:
//!
//! ```json
//! {"type": "data", "id": 42, "payload": {"index": 0}}
//! ```
//!
//! where `index` 0 means "from the beginning" and n means "from index n until
//! now", and the gateway answers with
//!
//! ```json
//! {"type": "data", "id": 42, "payload": {"latest_index": 1, "data": [{"timestamp": "...", "value1": 1.0, "value2": 2.0, "index": 1}]}}
//! ```
//!
//! The `CONNECT` request itself carries an `x-pq-gateway-id` header
//! identifying the gateway; see `CONNECT_PROTOCOL.md`'s "Gateway Identity and
//! History Resets" section for why (multiple gateways need a stable identity
//! to key persisted history and a pull cursor on) and for what
//! `payload.latest_index` is for.

use std::{
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
    task::{Context as TaskContext, Poll},
    time::Duration,
};

use anyhow::Context;
use axum::{
    Router,
    body::Body as AxumBody,
    http::{self, StatusCode},
    routing::post,
};
use bytes::Bytes;
use http_body::{Body, Frame};
use http_body_util::BodyExt;
use scion_quic::{
    h3::server::{H3Error, H3RequestBody, Http3Server, Http3ServerConfig, HttpService},
    quic::{
        config::QuicConfig,
        connection::ConnectionHandle,
        server_endpoint::{Metrics, QuicScionEndpointDriver, QuicScionServerEndpoint},
    },
    reexport::squiche,
    socket::GenericScionUdpSocket,
};
use serde_json::{Value, json};
use tokio::{
    sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel},
    time::{MissedTickBehavior, interval},
};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

use crate::storage::{Row, ServerStore, needs_reset};

/// Header carrying a gateway's stable identity on the CONNECT request, see
/// the module doc comment.
const GATEWAY_ID_HEADER: &str = "x-pq-gateway-id";

/// Identity used for a tunnel that did not send [`GATEWAY_ID_HEADER`] (an
/// older client, or a header that failed to decode as UTF-8).
const UNKNOWN_GATEWAY: &str = "unknown";

/// Path the server accepts POST requests on.
pub const DEFAULT_PATH: &str = "/edh/v1/hello";

/// TLS name of the server. HTTP/3 always runs over TLS, and the certificate below is
/// issued for this name.
pub const SERVER_NAME: &str = "pq-meter-server";

/// How often the server pulls new data from the gateway.
pub const PULL_INTERVAL: Duration = Duration::from_secs(5);

/// Serves the HTTP/3 application on `socket` until the process is stopped.
pub async fn serve(
    socket: Arc<dyn GenericScionUdpSocket>,
    path: &str,
    db_path: &Path,
) -> anyhow::Result<()> {
    let app = Router::new().route(path, post(receive));
    let config = quic_config().context("building the QUIC server configuration")?;
    let service = MeterService {
        app,
        db_path: db_path.to_path_buf(),
    };

    let metrics = Metrics::new_without_registry();
    let endpoint =
        QuicScionServerEndpoint::new([0u8; 32], config, socket.local_addr(), metrics);
    let driver = QuicScionEndpointDriver::with_config(
        endpoint,
        socket,
        |_connection: ConnectionHandle<Http3Server<MeterService>>| {},
        Http3ServerConfig::new(service),
    );

    driver
        .run(CancellationToken::new())
        .await
        .map_err(|error| anyhow::anyhow!("HTTP/3 server stopped: {error}"))
}

/// Prints the body of an incoming POST request.
///
/// Kept from the original template so the endpoint stays testable while the
/// tunnel protocol is being developed.
async fn receive(body: String) -> (StatusCode, &'static str) {
    println!("received: {body}");
    (StatusCode::OK, "ok\n")
}

/// The request handler: `CONNECT` opens a data tunnel, everything else goes to
/// the axum router.
struct MeterService {
    app: Router,
    db_path: PathBuf,
}

impl HttpService for MeterService {
    type Body = H3RequestBody;
    type ResponseBody = ResponseBody;

    async fn call(&self, req: http::Request<H3RequestBody>) -> http::Response<ResponseBody> {
        if req.method() == http::Method::CONNECT {
            return connect_tunnel(req, &self.db_path);
        }

        let response: http::Response<AxumBody> = self
            .app
            .clone()
            .oneshot(req)
            .await
            .expect("an axum router cannot fail");
        let (parts, body) = response.into_parts();
        let bytes = body
            .collect()
            .await
            .expect("collecting the response body")
            .to_bytes();
        http::Response::from_parts(parts, ResponseBody::Bytes(Some(bytes)))
    }
}

/// Opens a bidirectional data tunnel for a `CONNECT` request: answers with
/// `200`, then serves the tunnel with a background task while the response
/// body streams the server's data requests out to the gateway.
fn connect_tunnel(req: http::Request<H3RequestBody>, db_path: &PathBuf) -> http::Response<ResponseBody> {
    let gateway = req
        .headers()
        .get(GATEWAY_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
        .unwrap_or(UNKNOWN_GATEWAY)
        .to_string();
    if gateway == UNKNOWN_GATEWAY {
        tracing::warn!(
            "data tunnel opened with no {GATEWAY_ID_HEADER} header; its history will be \
             attributed to {UNKNOWN_GATEWAY:?} and may mix with another such gateway"
        );
    }
    println!("data tunnel opened by gateway {gateway:?}");

    let (_parts, body) = req.into_parts();
    let (out, rx) = unbounded_channel();
    tokio::spawn(tunnel_session(body, out, db_path.clone(), gateway));

    let mut response = http::Response::new(ResponseBody::Tunnel(rx));
    *response.status_mut() = StatusCode::OK;
    response
}

/// Serves one data tunnel: pulls data from the gateway every `PULL_INTERVAL`
/// and records what comes back.
///
/// The session ends when the gateway closes the tunnel, when the connection
/// drops, or when the response side is gone.
async fn tunnel_session(mut body: H3RequestBody, out: UnboundedSender<Bytes>, db_path: PathBuf, gateway: String) {
    let mut store = match ServerStore::open(&db_path) {
        Ok(store) => store,
        Err(err) => {
            eprintln!("could not open {db_path:?}: {err}");
            return;
        }
    };

    // The index the gateway has sent data up to so far; the next pull asks
    // for everything since it (0 = from the beginning). Resumed from this
    // gateway's last stored cursor, so a reconnect does not re-pull (and
    // double-count) its entire history from index 0 (TODO item 3).
    let mut last_index: u64 = match store.cursor(&gateway) {
        Ok(cursor) => cursor,
        Err(err) => {
            eprintln!("could not read the stored cursor for {gateway:?}: {err}");
            return;
        }
    };
    // Monotonic request id the gateway echoes back in its answers.
    let mut next_id: u64 = 1;
    // Bytes of the current gateway message not yet part of a full line.
    let mut pending = bytes::BytesMut::new();

    let mut ticker = interval(PULL_INTERVAL);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

    // Pull right away, so a fresh gateway delivers everything at once; the
    // interval's first tick is immediate.
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                if !send_pull_request(&out, next_id, last_index) {
                    return;
                }
                next_id += 1;
            }
            frame = std::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)) => {
                let Some(Ok(frame)) = frame else {
                    return;
                };
                let Ok(data) = frame.into_data() else {
                    return;
                };
                pending.extend_from_slice(&data);
                while let Some(line) = next_line(&mut pending) {
                    record_data(&line, &gateway, &mut last_index, &mut store);
                }
            }
        }
    }
}

/// Sends one data request for everything since index `from` over the tunnel.
/// Returns false if the tunnel's response side is gone.
fn send_pull_request(out: &UnboundedSender<Bytes>, id: u64, from: u64) -> bool {
    let message = json!({
        "type": "data",
        "id": id,
        "payload": {
            "index": from,
        },
    });
    let mut bytes = serde_json::to_vec(&message).expect("serializing the pull request");
    bytes.push(b'\n');
    out.send(Bytes::from(bytes)).is_ok()
}

/// Parses one gateway data reply, prints each measurement, and stores the
/// batch in `store`. Advances `last_index` to the highest index just stored,
/// or resets it to `0` if the gateway's own reported high-water mark
/// (`payload.latest_index`) shows its history has shrunk since the request
/// this reply answers — see [`needs_reset`] and `CONNECT_PROTOCOL.md`.
fn record_data(line: &[u8], gateway: &str, last_index: &mut u64, store: &mut ServerStore) {
    let message: Value = match serde_json::from_slice(line) {
        Ok(message) => message,
        Err(err) => {
            eprintln!("invalid JSON from gateway {gateway:?}, ignoring: {err}");
            return;
        }
    };
    if message.get("type").and_then(Value::as_str) != Some("data") {
        return;
    }
    let Some(data) = message.pointer("/payload/data").and_then(Value::as_array) else {
        eprintln!("data reply from gateway {gateway:?} has no \"payload.data\" array, ignoring");
        return;
    };
    let latest_index = message.pointer("/payload/latest_index").and_then(Value::as_u64);

    let mut rows = Vec::with_capacity(data.len());
    let mut new_last_index = *last_index;
    for measurement in data {
        println!("data: {measurement}");
        match measurement_to_row(measurement) {
            Some(row) => {
                new_last_index = new_last_index.max(row.idx);
                rows.push(row);
            }
            None => eprintln!(
                "measurement from gateway {gateway:?} is missing a numeric \"index\" or a \
                 parseable \"timestamp\", skipping it: {measurement}"
            ),
        }
    }

    match store.record_batch(gateway, new_last_index, &rows) {
        Ok(recorded) => println!(
            "received {} measurement(s) from gateway {gateway:?} ({} new, {} already stored)",
            rows.len(),
            recorded.inserted,
            recorded.duplicates,
        ),
        Err(err) => {
            // Leave last_index untouched: the transaction rolled back, so
            // nothing was persisted, and the next pull should ask for this
            // same range again rather than skip past it.
            eprintln!("failed to store data from gateway {gateway:?}, will retry: {err}");
            return;
        }
    }

    if needs_reset(*last_index, latest_index) {
        println!(
            "gateway {gateway:?} reports a smaller history than before (latest_index \
             {latest_index:?} < cursor {last_index}); its local database was likely deleted and \
             recreated — resetting its pull cursor to 0"
        );
        *last_index = 0;
    } else {
        *last_index = new_last_index;
    }
}

/// Builds one [`Row`] from a measurement object, or `None` if it lacks a
/// numeric `"index"` or a parseable `"timestamp"`. Every other numeric field
/// becomes a value; `"id"` (the client's own row id, redundant with
/// `"index"`) is skipped, and a field whose name cannot be a SQL column name
/// is dropped with a warning rather than losing the whole measurement to it.
fn measurement_to_row(measurement: &Value) -> Option<Row> {
    let idx = measurement.get("index").and_then(Value::as_u64)?;
    let ts_millis = parse_ts_millis(measurement.get("timestamp"))?;

    let values = measurement
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(name, _)| !matches!(name.as_str(), "index" | "timestamp" | "id"))
        .filter_map(|(name, value)| {
            let value = value.as_f64()?;
            if crate::storage::is_valid_value_name(name) {
                Some((name.clone(), value))
            } else {
                eprintln!("measurement {idx} has an unusable field name {name:?}, dropping it");
                None
            }
        })
        .collect();

    Some(Row { idx, ts_millis, values })
}

/// Parses a `"timestamp"` field as milliseconds since the Unix epoch. The
/// client sends it as a string (see `pq-meter-client`'s
/// `stored_reading_json`), but a bare JSON number is accepted too.
fn parse_ts_millis(value: Option<&Value>) -> Option<i64> {
    match value? {
        Value::String(s) => s.parse().ok(),
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        _ => None,
    }
}

/// Splits one newline-terminated message off the front of `pending`, if the
/// bytes for it have all arrived.
fn next_line(pending: &mut bytes::BytesMut) -> Option<Vec<u8>> {
    let end = pending.iter().position(|&byte| byte == b'\n')?;
    Some(pending.split_to(end + 1).to_vec())
}

/// The response body of every request: either the bytes of an axum response
/// (one frame, then done) or the tunnel that streams the server's data
/// requests until the session ends.
enum ResponseBody {
    /// A finite body: the (collected) answer of the axum router.
    Bytes(Option<Bytes>),
    /// The server side of a data tunnel: one frame per data request.
    Tunnel(UnboundedReceiver<Bytes>),
}

impl Body for ResponseBody {
    type Data = Bytes;
    type Error = H3Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        match this {
            ResponseBody::Bytes(bytes) => match bytes.take() {
                Some(bytes) => Poll::Ready(Some(Ok(Frame::data(bytes)))),
                None => Poll::Ready(None),
            },
            ResponseBody::Tunnel(rx) => match rx.poll_recv(cx) {
                Poll::Ready(Some(bytes)) => Poll::Ready(Some(Ok(Frame::data(bytes)))),
                Poll::Ready(None) => Poll::Ready(None),
                Poll::Pending => Poll::Pending,
            },
        }
    }
}

/// Builds the QUIC configuration of the server, with a self-signed certificate that is
/// generated on every start.
///
/// The client does not verify this certificate. That keeps the setup short, but it means
/// the connection is encrypted without the client knowing who it talks to. Use a real
/// certificate before taking anything like this outside of a hackathon.
fn quic_config() -> anyhow::Result<squiche::Config> {
    let mut config = QuicConfig::builder()
        .verify_peer(false)
        .build()
        .to_quiche_config()
        .context("creating the QUIC configuration")?;

    let certificate = rcgen::generate_simple_self_signed(vec![SERVER_NAME.to_string()])
        .context("generating a self-signed certificate")?;

    // squiche reads the certificate and the key from files, so write them to temporary
    // files that are removed again when this function returns.
    let certificate_file = write_temporary_file(certificate.cert.pem().as_bytes(), "certificate")?;
    let key_file = write_temporary_file(certificate.signing_key.serialize_pem().as_bytes(), "key")?;

    config
        .load_cert_chain_from_pem_file(path_of(&certificate_file)?)
        .context("loading the certificate")?;
    config
        .load_priv_key_from_pem_file(path_of(&key_file)?)
        .context("loading the private key")?;

    Ok(config)
}

/// Writes `contents` to a temporary file.
fn write_temporary_file(contents: &[u8], what: &str) -> anyhow::Result<tempfile::NamedTempFile> {
    use std::io::Write;

    let mut file = tempfile::NamedTempFile::new()
        .with_context(|| format!("creating a temporary file for the {what}"))?;
    file.write_all(contents)
        .with_context(|| format!("writing the {what}"))?;
    Ok(file)
}

/// Returns the path of a temporary file as a string.
fn path_of(file: &tempfile::NamedTempFile) -> anyhow::Result<&str> {
    file.path()
        .to_str()
        .context("temporary file path is not valid UTF-8")
}
