//! Client side of the PQ Meter CONNECT protocol.
//!
//! With no subcommand, opens a persistent HTTP/3 `CONNECT` tunnel to
//! `pq-meter-server` and answers its `"data"` requests with newline-delimited
//! JSON (NDJSON), reconnecting with bounded backoff whenever the tunnel
//! closes. See `CONNECT_PROTOCOL.md` at the repository root for the wire
//! protocol this implements.
//!
//! Every reading answered over the tunnel is also appended to a local SQLite
//! file (see [`storage`]), so history survives a restart and can be inspected
//! without the server. `show` prints that history back — the newest few, or
//! everything after a given row id — as a table or as JSON lines.
//!
//! The meter itself is read through the [`meter::MeterSource`] trait; `main`
//! wires in [`meter::umg605::ModbusMeter`], or [`meter::dummy::DummyMeter`] when
//! `--dummy-meter` is passed (see `scripts/run-dummy.sh --live`).

mod meter;
// `show` uses most of `storage`; `query` (time-range reads) is exercised only
// by the storage tests and waits for a query endpoint on the server.
#[allow(dead_code)]
mod storage;

use std::{
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, bail};
use clap::Parser;
use meter::MeterSource;
use scion_quic::{
    h3::client::{H3DuplexStream, Http3Client},
    quic::config::QuicConfig,
    socket::GenericScionUdpSocket,
};
use scion_stack::ScionStackBuilder;
use sciparse::address::ip_socket_addr::ScionSocketIpAddr;
use serde_json::{Value, json};
use storage::{MeterStore, StoredReading};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use url::Url;

/// TLS name the server's certificate is issued for.
const SERVER_NAME: &str = "pq-meter-server";

/// Largest NDJSON line accepted from the server before the tunnel is closed.
const MAX_LINE_SIZE: usize = 64 * 1024;

/// Backoff applied between reconnect attempts, doubling up to the maximum.
const INITIAL_BACKOFF: Duration = Duration::from_millis(500);
const MAX_BACKOFF: Duration = Duration::from_secs(30);

// With no subcommand the client runs `TunnelArgs` (opening the CONNECT
// tunnel); `show` reads the SQLite file instead and does not need the SCION
// addresses. The doc comment is deliberately a plain comment so clap shows
// only the `about` string below, not this note.
#[derive(Debug, Parser)]
#[command(
    version,
    about = "Maintains a CONNECT tunnel to pq-meter-server and answers its data requests, or shows stored readings",
    args_conflicts_with_subcommands = true,
    subcommand_negates_reqs = true
)]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,

    #[command(flatten)]
    tunnel: Option<TunnelArgs>,
}

#[derive(Debug, clap::Subcommand)]
enum Command {
    /// Poll the Modbus meter and append readings to the local SQLite file.
    Record(RecordArgs),
    /// Print stored readings from the local SQLite file.
    Show(ShowArgs),
}

/// Arguments for the default behaviour: opening the CONNECT tunnel.
#[derive(Debug, clap::Args)]
struct TunnelArgs {
    /// URL of the endhost API this client attaches to, for example
    /// `http://192.168.1.42:31000`.
    #[arg(long)]
    endhost_api: Url,

    /// SCION address of the server, for example `[2-ff00:0:212,192.168.1.42]:31337`.
    #[arg(long)]
    server: ScionSocketIpAddr,

    /// IP address of the UMG 605-PRO Modbus TCP meter.
    #[arg(long, required_unless_present = "dummy_meter", conflicts_with = "dummy_meter")]
    meter_ip: Option<IpAddr>,

    /// Modbus TCP port of the meter.
    #[arg(long, default_value_t = umg605_modbus_client::DEFAULT_MODBUS_PORT)]
    meter_port: u16,

    /// Modbus unit ID configured on the meter.
    #[arg(long, default_value_t = 1)]
    meter_unit: u8,

    /// Connection and individual register-read timeout in seconds.
    #[arg(long, default_value_t = 5)]
    meter_timeout_secs: u64,

    /// Use a simulated meter with plausible drifting values instead of
    /// connecting to real hardware, for demos and tests (see
    /// `scripts/run-dummy.sh --live`).
    #[arg(long)]
    dummy_meter: bool,

    /// SQLite file every answered reading is also appended to. Created if it
    /// does not exist.
    #[arg(long, default_value = "pqmeter.db")]
    db: PathBuf,
}

/// Arguments for recording meter readings without a SCION connection.
#[derive(Debug, clap::Args)]
struct RecordArgs {
    /// IP address of the UMG 605-PRO Modbus TCP meter.
    #[arg(long)]
    meter_ip: IpAddr,

    /// Modbus TCP port of the meter.
    #[arg(long, default_value_t = umg605_modbus_client::DEFAULT_MODBUS_PORT)]
    meter_port: u16,

    /// Modbus unit ID configured on the meter.
    #[arg(long, default_value_t = 1)]
    meter_unit: u8,

    /// Connection and individual register-read timeout in seconds.
    #[arg(long, default_value_t = 5)]
    meter_timeout_secs: u64,

    /// SQLite file readings are appended to. Created if it does not exist.
    #[arg(long, default_value = "pqmeter.db")]
    db: PathBuf,

    /// Number of seconds between meter reads.
    #[arg(long, default_value_t = 1.0, value_parser = parse_positive_seconds)]
    interval: f64,
}

fn parse_positive_seconds(value: &str) -> Result<f64, String> {
    let seconds: f64 = value
        .parse()
        .map_err(|_| "must be a number of seconds".to_string())?;
    if seconds.is_finite() && seconds > 0.0 {
        Ok(seconds)
    } else {
        Err("must be greater than zero".to_string())
    }
}

/// Arguments for `show`.
#[derive(Debug, clap::Args)]
struct ShowArgs {
    /// SQLite file to read.
    #[arg(long, default_value = "pqmeter.db")]
    db: PathBuf,

    /// Print the most recent N readings, newest first.
    #[arg(long, default_value_t = 20, conflicts_with = "since_id")]
    last: usize,

    /// Instead print readings with a row id greater than this, oldest first.
    #[arg(long)]
    since_id: Option<i64>,

    /// Cap on how many rows `--since-id` prints.
    #[arg(long, default_value_t = 10_000)]
    limit: usize,

    /// Print one JSON object per line instead of a table.
    #[arg(long)]
    json: bool,
}

/// The gateway's persistent state: the meter that records readings locally
/// and the database from which tunnel responses are served. Outlives any
/// single tunnel, so reconnects use the same cursorable history.
struct Gateway {
    meter: Box<dyn MeterSource>,
    store: MeterStore,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args = Args::parse();

    match (args.command, args.tunnel) {
        (Some(Command::Record(record_args)), _) => record(record_args).await,
        (Some(Command::Show(show_args)), _) => show(show_args),
        (None, Some(tunnel_args)) => run(tunnel_args).await,
        (None, None) => Err(anyhow::anyhow!(
            "nothing to do: pass --endhost-api and --server to open the tunnel, or run `show`. See --help."
        )),
    }
}

/// Polls the physical meter and persists every successful reading locally.
async fn record(args: RecordArgs) -> anyhow::Result<()> {
    let meter_addr = SocketAddr::new(args.meter_ip, args.meter_port);
    let mut meter = meter::umg605::ModbusMeter::connect(
        meter_addr,
        args.meter_unit,
        Duration::from_secs(args.meter_timeout_secs),
    )
    .await
    .with_context(|| format!("connecting to the Modbus meter at {meter_addr}"))?;
    let store = MeterStore::open(&args.db)
        .with_context(|| format!("opening the database at {}", args.db.display()))?;
    let interval = Duration::from_secs_f64(args.interval);
    tracing::info!(
        meter = %meter_addr,
        kind = meter.kind(),
        database = %args.db.display(),
        ?interval,
        "recording meter readings"
    );

    let mut ticker = tokio::time::interval(interval);
    loop {
        ticker.tick().await;
        match meter.read_snapshot().await {
            Ok(snapshot) => {
                let readings = snapshot
                    .readings
                    .iter()
                    .map(|r| format!("{}={:.2}", r.name, r.value))
                    .collect::<Vec<_>>()
                    .join(", ");
                match store.insert_now(snapshot.into()) {
                    Ok(()) => tracing::info!(%readings, "stored meter reading"),
                    Err(err) => tracing::warn!(error = ?err, "failed to persist meter reading"),
                }
            }
            Err(err) => tracing::warn!(error = ?err, "failed to read the meter"),
        }
    }
}

/// Opens the CONNECT tunnel and serves data requests on it, reconnecting with
/// backoff until the process is stopped.
async fn run(args: TunnelArgs) -> anyhow::Result<()> {
    // The SDK uses rustls for its control plane; pick a crypto backend.
    scion_sdk_utils::rustls::select_ring_crypto_provider();

    let socket = build_socket(&args.endhost_api)
        .await
        .context("setting up the SCION stack")?;

    // The server uses a self-signed certificate, so its identity is not verified.
    let client = Http3Client::with_config(
        args.server,
        socket,
        Some(SERVER_NAME.to_string()),
        QuicConfig::builder().verify_peer(false).build(),
    );
    let authority = format!("{SERVER_NAME}:{}", args.server.port());

    let store = MeterStore::open(&args.db)
        .with_context(|| format!("opening the database at {}", args.db.display()))?;
    let meter: Box<dyn MeterSource> = if args.dummy_meter {
        Box::new(meter::dummy::DummyMeter::new())
    } else {
        // `required_unless_present = "dummy_meter"` guarantees this is `Some`.
        let meter_addr = SocketAddr::new(args.meter_ip.unwrap(), args.meter_port);
        let modbus_meter = meter::umg605::ModbusMeter::connect(
            meter_addr,
            args.meter_unit,
            Duration::from_secs(args.meter_timeout_secs),
        )
        .await
        .with_context(|| format!("connecting to the Modbus meter at {meter_addr}"))?;
        Box::new(modbus_meter)
    };
    let meter_kind = meter.kind();
    let mut gateway = Gateway { meter, store };

    let mut backoff = INITIAL_BACKOFF;
    loop {
        tracing::info!(server = %args.server, kind = meter_kind, "opening tunnel");
        match run_tunnel(&client, &authority, &mut gateway).await {
            Ok(()) => {
                tracing::info!("tunnel closed, reconnecting");
                backoff = INITIAL_BACKOFF;
            }
            Err(err) => {
                tracing::warn!(error = ?err, delay = ?backoff, "tunnel failed, reconnecting");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        }
    }
}

/// Builds the SCION socket the client sends and receives packets on.
async fn build_socket(endhost_api: &Url) -> anyhow::Result<Arc<dyn GenericScionUdpSocket>> {
    let stack = ScionStackBuilder::new()
        .with_endhost_api(endhost_api.clone())
        // A dummy token. Normally a client asks the AA (the authentication and
        // authorization service) for a SNAP token; here the AA is left out.
        .with_auth_token(snap_tokens::v0::dummy_snap_token())
        .build()
        .await
        .context("building the SCION stack")?;
    let socket = stack.bind(None).await.context("opening a SCION socket")?;
    Ok(Arc::new(socket))
}

/// Opens one `CONNECT` tunnel and hands the resulting byte stream to
/// [`serve_tunnel`].
async fn run_tunnel(
    client: &Http3Client,
    authority: &str,
    gateway: &mut Gateway,
) -> anyhow::Result<()> {
    let request = http::Request::builder()
        .method(http::Method::CONNECT)
        .uri(format!("https://{authority}"))
        .body(())
        .context("building the CONNECT request")?;

    let (response, writer) = client
        .request_with_writer(request)
        .await
        .context("sending the CONNECT request")?;
    let response = response.await.context("awaiting the CONNECT response")?;
    anyhow::ensure!(
        response.status().is_success(),
        "server refused the tunnel with {}",
        response.status()
    );

    let stream = H3DuplexStream::new(writer, response.into_body());
    serve_tunnel(stream, gateway).await
}

/// Frames NDJSON off `stream` and answers each message, until the peer closes
/// its write half or a framing error closes the tunnel. Generic over the byte
/// stream so it can be driven over an in-memory pipe in tests, with no network.
async fn serve_tunnel<S>(mut stream: S, gateway: &mut Gateway) -> anyhow::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];

    loop {
        let n = stream
            .read(&mut chunk)
            .await
            .context("reading from the tunnel")?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);

        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            let line = buf[..pos].to_vec();
            buf.drain(..=pos);
            handle_line(&mut stream, &line, gateway).await?;
        }

        anyhow::ensure!(
            buf.len() <= MAX_LINE_SIZE,
            "line exceeds {MAX_LINE_SIZE} bytes without a delimiter"
        );
    }
}

/// Parses one NDJSON line, answers it, and writes the reply to `stream`.
async fn handle_line<S>(stream: &mut S, line: &[u8], gateway: &mut Gateway) -> anyhow::Result<()>
where
    S: AsyncWrite + Unpin,
{
    let request: Value = match serde_json::from_slice(line) {
        Ok(value) => value,
        Err(err) => bail!("invalid JSON line: {err}"),
    };

    let Some(id) = request.get("id").and_then(Value::as_u64) else {
        bail!("message is missing a numeric \"id\"");
    };
    let kind = request
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();

    let reply = match kind {
        "data" => {
            let from_index = request
                .pointer("/payload/index")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            tracing::debug!(id, from_index, "answering data request from local history");
            match gateway.meter.read_snapshot().await {
                Ok(snapshot) => {
                    // Local persistence is a side channel: a failure to store the
                    // reading is logged, not surfaced as a protocol error, so the
                    // server still gets its reply either way.
                    if let Err(err) = gateway.store.insert_now(snapshot.into()) {
                        tracing::warn!(id, error = ?err, "failed to persist reading");
                    }

                    match gateway.store.since_id(from_index as i64, 10_000) {
                        Ok(readings) => json!({
                            "type": "data",
                            "id": id,
                            "payload": {
                                "data": readings.iter().map(stored_reading_json).collect::<Vec<_>>(),
                            },
                        }),
                        Err(err) => {
                            tracing::warn!(id, error = ?err, "failed to read stored readings");
                            json!({
                                "type": "error",
                                "id": id,
                                "payload": { "message": "unable to load stored readings" },
                            })
                        }
                    }
                }
                Err(err) => {
                    tracing::warn!(id, error = ?err, "meter read failed");
                    json!({
                        "type": "error",
                        "id": id,
                        "payload": { "message": format!("unable to read the meter: {err:#}") },
                    })
                }
            }
        }
        other => {
            tracing::warn!(id, kind = other, "unsupported message type");
            json!({
                "type": "error",
                "id": id,
                "payload": { "message": format!("unsupported message type \"{other}\"") },
            })
        }
    };

    write_line(stream, &reply).await
}

/// Writes one NDJSON message, terminated by `\n`, and flushes it.
async fn write_line<S>(stream: &mut S, message: &Value) -> anyhow::Result<()>
where
    S: AsyncWrite + Unpin,
{
    let mut bytes = serde_json::to_vec(message).context("encoding reply")?;
    bytes.push(b'\n');
    stream.write_all(&bytes).await.context("writing reply")?;
    stream.flush().await.context("flushing reply")?;
    Ok(())
}

/// Prints stored readings: `--since-id` for everything after a row id (oldest
/// first), otherwise the most recent `--last` (newest first).
fn show(args: ShowArgs) -> anyhow::Result<()> {
    let store = MeterStore::open(&args.db)
        .with_context(|| format!("opening the database at {}", args.db.display()))?;
    let rows = select_rows(&store, &args)?;

    if args.json {
        for row in &rows {
            println!("{}", row_json(row));
        }
    } else {
        print_table(&rows);
    }
    Ok(())
}

/// Chooses which stored readings `show` prints, based on the arguments.
fn select_rows(store: &MeterStore, args: &ShowArgs) -> anyhow::Result<Vec<StoredReading>> {
    match args.since_id {
        Some(after_id) => store.since_id(after_id, args.limit),
        None => store.latest(args.last),
    }
}

fn row_json(row: &StoredReading) -> String {
    stored_reading_json(row).to_string()
}

/// Renders a stored reading in the wire format, using its SQLite row id as
/// the incremental response cursor.
fn stored_reading_json(row: &StoredReading) -> Value {
    let mut object = serde_json::Map::with_capacity(row.reading.values.len() + 3);
    object.insert("id".to_string(), json!(row.id));
    object.insert("timestamp".to_string(), json!(row.reading.ts_millis.to_string()));
    for (name, value) in &row.reading.values {
        object.insert(name.clone(), json!(value));
    }
    object.insert("index".to_string(), json!(row.id));
    Value::Object(object)
}

/// Prints one row per reading, with one column per value name. Every row is
/// expected to carry the same names (one database file holds one meter kind),
/// so the header is taken from the first row.
fn print_table(rows: &[StoredReading]) {
    let Some(names) = rows.first().map(|row| {
        row.reading
            .values
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>()
    }) else {
        return;
    };

    print!("{:>8}  {:<19}", "id", "time (UTC)");
    for name in &names {
        print!("  {name:>14}");
    }
    println!();

    for row in rows {
        let time = chrono::DateTime::from_timestamp_millis(row.reading.ts_millis)
            .map(|dt| dt.format("%Y-%m-%d %H:%M:%S").to_string())
            .unwrap_or_else(|| row.reading.ts_millis.to_string());
        print!("{:>8}  {:<19}", row.id, time);
        for (_, value) in &row.reading.values {
            print!("  {value:>14.2}");
        }
        println!();
    }
}

/// Bridges the meter's own reading type to the one `storage` persists.
/// `ts_millis` is left at zero; [`MeterStore::insert_now`] stamps it.
impl From<meter::MeterSnapshot> for storage::Reading {
    fn from(snapshot: meter::MeterSnapshot) -> Self {
        storage::Reading {
            ts_millis: 0,
            values: snapshot
                .readings
                .into_iter()
                .map(|r| (r.name.to_string(), r.value))
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::AsyncWriteExt;

    use super::*;
    use crate::meter::MeterSnapshot;

    fn test_gateway() -> Gateway {
        Gateway {
            meter: Box::new(meter::dummy::DummyMeter::new()),
            store: MeterStore::open(std::path::Path::new(":memory:")).expect("in-memory store"),
        }
    }

    /// Encodes `requests` as NDJSON, one per line.
    fn ndjson(requests: &[Value]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for request in requests {
            bytes.extend(serde_json::to_vec(request).unwrap());
            bytes.push(b'\n');
        }
        bytes
    }

    /// Writes `input` into one half of an in-memory pipe and shuts that half
    /// down, so `serve_tunnel` sees EOF once it has drained everything
    /// already written, then runs the loop to completion and returns its
    /// result plus everything it wrote back.
    ///
    /// The pipe is sized well above `MAX_LINE_SIZE` so a test writing an
    /// oversized line cannot deadlock against a buffer nobody has started
    /// draining yet (everything is written before `serve_tunnel` is polled).
    async fn serve_input(gateway: &mut Gateway, input: &[u8]) -> (anyhow::Result<()>, String) {
        let (mut peer, tunnel) = tokio::io::duplex(4 * MAX_LINE_SIZE);
        peer.write_all(input).await.unwrap();
        peer.shutdown().await.unwrap();

        let result = serve_tunnel(tunnel, gateway).await;

        let mut replies = String::new();
        tokio::io::AsyncReadExt::read_to_string(&mut peer, &mut replies)
            .await
            .unwrap();
        (result, replies)
    }

    fn parse_replies(replies: &str) -> Vec<Value> {
        replies
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[tokio::test]
    async fn data_request_is_answered_with_stored_readings() {
        let mut gateway = test_gateway();
        let input = ndjson(&[json!({"type": "data", "id": 42, "payload": {}})]);
        let (result, replies) = serve_input(&mut gateway, &input).await;
        result.expect("serve_tunnel should exit cleanly on EOF");
        println!("dummy data reply: {}", replies.trim_end());

        let reply = &parse_replies(&replies)[0];
        assert_eq!(reply["type"], "data");
        assert_eq!(reply["id"], 42);
        let data = reply["payload"]["data"].as_array().expect("data array");
        assert_eq!(data.len(), 1);
        assert!(data[0]["voltage_l1_v"].is_number());
        assert_eq!(data[0]["index"], 1);
    }

    #[tokio::test]
    async fn data_request_is_also_persisted_to_the_store() {
        let mut gateway = test_gateway();
        let input = ndjson(&[json!({"type": "data", "id": 1, "payload": {}})]);
        serve_input(&mut gateway, &input).await.0.unwrap();

        let stored = gateway.store.latest(10).unwrap();
        assert_eq!(stored.len(), 1);
        assert!(stored[0].reading.value("voltage_l1_v").unwrap() > 0.0);
    }

    #[tokio::test]
    async fn data_request_returns_only_rows_after_its_index() {
        let mut gateway = test_gateway();
        for ts in [100, 200, 300] {
            gateway.store.insert(&sample(ts)).unwrap();
        }
        let input = ndjson(&[json!({"type": "data", "id": 1, "payload": {"index": 1}})]);
        let (_, replies) = serve_input(&mut gateway, &input).await;

        let data = parse_replies(&replies)[0]["payload"]["data"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(data.len(), 3);
        assert_eq!(data[0]["index"], 2);
        assert_eq!(data[1]["index"], 3);
        assert_eq!(data[2]["index"], 4);
    }

    #[tokio::test]
    async fn index_increases_across_requests() {
        let mut gateway = test_gateway();
        let input = ndjson(&[
            json!({"type": "data", "id": 1, "payload": {}}),
            json!({"type": "data", "id": 2, "payload": {"index": 1}}),
        ]);
        let (result, replies) = serve_input(&mut gateway, &input).await;
        result.expect("serve_tunnel should exit cleanly on EOF");

        let lines = parse_replies(&replies);
        assert_eq!(lines[0]["payload"]["data"][0]["index"], 1);
        assert_eq!(lines[1]["payload"]["data"][0]["index"], 2);
    }

    #[tokio::test]
    async fn unknown_type_is_answered_with_an_error() {
        let mut gateway = test_gateway();
        let input = ndjson(&[
            json!({"type": "reboot", "id": 7, "payload": {}}),
            json!({"type": "data", "id": 8, "payload": {}}),
        ]);
        let (result, replies) = serve_input(&mut gateway, &input).await;
        result.expect("an unknown type should not close the tunnel");

        let lines = parse_replies(&replies);
        assert_eq!(lines[0]["type"], "error");
        assert_eq!(lines[0]["id"], 7);
        assert!(lines[0]["payload"]["message"].is_string());
        // The tunnel stays open after an unknown type: the next request is
        // still answered.
        assert_eq!(lines[1]["type"], "data");
    }

    /// A meter that always fails, to prove a read failure reaches the wire as
    /// the protocol's `"error"` reply instead of the tunnel just dying.
    struct BrokenMeter;

    #[async_trait::async_trait]
    impl MeterSource for BrokenMeter {
        fn kind(&self) -> &'static str {
            "broken"
        }

        async fn read_snapshot(&mut self) -> anyhow::Result<MeterSnapshot> {
            anyhow::bail!("meter did not respond")
        }
    }

    #[tokio::test]
    async fn meter_failure_is_answered_with_an_error() {
        let mut gateway = Gateway {
            meter: Box::new(BrokenMeter),
            store: MeterStore::open(std::path::Path::new(":memory:")).expect("in-memory store"),
        };
        let input = ndjson(&[json!({"type": "data", "id": 1, "payload": {}})]);
        let (result, replies) = serve_input(&mut gateway, &input).await;
        result.expect("a meter failure should not close the tunnel");

        let reply = &parse_replies(&replies)[0];
        assert_eq!(reply["type"], "error");
        assert_eq!(reply["id"], 1);
        let message = reply["payload"]["message"]
            .as_str()
            .expect("message string");
        assert!(message.contains("meter did not respond"));
    }

    #[tokio::test]
    async fn invalid_json_closes_the_tunnel() {
        let mut gateway = test_gateway();
        let (result, _) = serve_input(&mut gateway, b"not json\n").await;
        assert!(result.is_err(), "invalid JSON should close the tunnel");
    }

    #[tokio::test]
    async fn oversized_line_closes_the_tunnel() {
        let mut gateway = test_gateway();
        // No delimiter, so the whole thing accumulates as one unterminated line.
        let oversized = vec![b'a'; MAX_LINE_SIZE + 1];
        let (result, _) = serve_input(&mut gateway, &oversized).await;
        assert!(result.is_err(), "an oversized line should close the tunnel");
    }

    #[test]
    fn bare_invocation_is_the_tunnel_command() {
        let args = Args::try_parse_from([
            "pq-meter-client",
            "--endhost-api",
            "http://127.0.0.1:31000/",
            "--server",
            "[2-ff00:0:212,127.0.0.1]:59218",
            "--meter-ip",
            "192.168.1.50",
        ])
        .unwrap();

        assert!(args.command.is_none());
        let tunnel = args.tunnel.expect("tunnel args parsed");
        assert_eq!(tunnel.db, PathBuf::from("pqmeter.db"));
        assert_eq!(tunnel.meter_ip.unwrap().to_string(), "192.168.1.50");
    }

    #[test]
    fn dummy_meter_flag_does_not_require_meter_ip() {
        let args = Args::try_parse_from([
            "pq-meter-client",
            "--endhost-api",
            "http://127.0.0.1:31000/",
            "--server",
            "[2-ff00:0:212,127.0.0.1]:59218",
            "--dummy-meter",
        ])
        .unwrap();

        let tunnel = args.tunnel.expect("tunnel args parsed");
        assert!(tunnel.dummy_meter);
        assert_eq!(tunnel.meter_ip, None);
    }

    #[test]
    fn meter_ip_and_dummy_meter_are_mutually_exclusive() {
        assert!(
            Args::try_parse_from([
                "pq-meter-client",
                "--endhost-api",
                "http://127.0.0.1:31000/",
                "--server",
                "[2-ff00:0:212,127.0.0.1]:59218",
                "--meter-ip",
                "192.168.1.50",
                "--dummy-meter",
            ])
            .is_err()
        );
    }

    #[test]
    fn neither_meter_ip_nor_dummy_meter_is_rejected() {
        assert!(
            Args::try_parse_from([
                "pq-meter-client",
                "--endhost-api",
                "http://127.0.0.1:31000/",
                "--server",
                "[2-ff00:0:212,127.0.0.1]:59218",
            ])
            .is_err()
        );
    }

    #[test]
    fn show_does_not_require_the_scion_addresses() {
        assert!(Args::try_parse_from(["pq-meter-client", "show"]).is_ok());
    }

    #[test]
    fn show_rejects_mixing_in_the_tunnel_flags() {
        // args_conflicts_with_subcommands: can't do both at once.
        assert!(
            Args::try_parse_from([
                "pq-meter-client",
                "--server",
                "[2-ff00:0:212,127.0.0.1]:1",
                "show",
            ])
            .is_err()
        );
    }

    #[test]
    fn show_defaults_to_the_last_twenty() {
        let args = Args::try_parse_from(["pq-meter-client", "show"]).unwrap();

        let Some(Command::Show(show)) = args.command else {
            panic!("expected the show command, got {:?}", args.command);
        };
        assert_eq!(show.last, 20);
        assert_eq!(show.since_id, None);
        assert_eq!(show.db, PathBuf::from("pqmeter.db"));
        assert!(!show.json);
    }

    #[test]
    fn show_rejects_last_together_with_since_id() {
        assert!(
            Args::try_parse_from(["pq-meter-client", "show", "--last", "5", "--since-id", "10"])
                .is_err()
        );
    }

    #[test]
    fn select_rows_follows_since_id_when_set() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pqmeter.db");
        let store = MeterStore::open(&path).unwrap();
        for _ in 0..5 {
            store.insert_now(sample(0)).unwrap();
        }

        let args = ShowArgs {
            db: path,
            last: 20,
            since_id: Some(2),
            limit: 100,
            json: false,
        };
        let ids: Vec<i64> = select_rows(&store, &args)
            .unwrap()
            .iter()
            .map(|r| r.id)
            .collect();

        assert_eq!(ids, vec![3, 4, 5]);
    }

    #[test]
    fn select_rows_defaults_to_the_last_n_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pqmeter.db");
        let store = MeterStore::open(&path).unwrap();
        for _ in 0..5 {
            store.insert_now(sample(0)).unwrap();
        }

        let args = ShowArgs {
            db: path,
            last: 2,
            since_id: None,
            limit: 10_000,
            json: false,
        };
        let ids: Vec<i64> = select_rows(&store, &args)
            .unwrap()
            .iter()
            .map(|r| r.id)
            .collect();

        assert_eq!(ids, vec![5, 4]);
    }

    fn sample(ts_millis: i64) -> storage::Reading {
        storage::Reading {
            ts_millis,
            values: vec![
                ("voltage_l1_v".to_string(), 230.0),
                ("current_l1_a".to_string(), 1.8),
                ("active_power_l1_w".to_string(), 414.0),
                ("reactive_power_l1_var".to_string(), 12.0),
                ("phase_angle_l1_deg".to_string(), 3.0),
            ],
        }
    }
}
