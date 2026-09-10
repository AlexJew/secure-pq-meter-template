//! Client side of the Energy Data Hackdays gateway challenge. Runs on the gateway (the
//! Raspberry Pi); `pq-meter-server` runs on the laptop.
//!
//! Two things it can do:
//!
//! * with no subcommand, it sends one message to `pq-meter-server` over SCION and prints
//!   the answer. The work is done by [`scion_http3::Client`], the high-level HTTP/3 client
//!   of the SDK, which keeps a pool of connections so a program that sends in a loop pays
//!   for the connection only once. It needs two addresses:
//!   * `--endhost-api`: the URL of the endhost API of its own AS, where the client asks for
//!     paths and for the SNAP that carries its packets. The server prints this URL.
//!   * `--server`: the SCION address of the HTTP/3 server, also printed by the server.
//! * `record` reads the meter over Modbus TCP on an interval and appends each reading to a
//!   local SQLite file (see [`storage`]). This is what runs on the Pi to build up history.
//! * `show` prints readings back from that file — the newest few, or everything after a
//!   given row id — as a table or as JSON lines.

// `record`/`show` use most of `storage`; `query` (time-range reads) is exercised only by
// the storage tests and waits for the query endpoint.
#[allow(dead_code)]
mod storage;

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use clap::Parser;
use scion_http3::{Client, Config, Request, scion_quic::quic::config::QuicConfig};
use sciparse::address::ip_socket_addr::ScionSocketIpAddr;
use umg605_modbus_client::{DEFAULT_MODBUS_PORT, ModbusUnit, ReadError, Umg605ProClient};
use url::Url;

use storage::{MeterStore, Reading, StoredReading};

/// TLS name the server's certificate is issued for.
const SERVER_NAME: &str = "pq-meter-server";

/// Largest response body we read. The answer of the server is a few bytes.
const MAX_BODY_SIZE: usize = 1024;

/// Unit id for a directly addressed Modbus TCP device, per the Modbus TCP spec.
const DEFAULT_MODBUS_UNIT: u8 = 1;

// Command line arguments. With no subcommand the client runs `SendArgs` (the original
// behaviour), so the invocations in the README keep working; `record` takes over
// completely and does not need the SCION addresses. The doc comment is deliberately a
// plain comment so clap shows only the `about` string below, not this note.
#[derive(Debug, Parser)]
#[command(
    version,
    about = "Sends meter data to pq-meter-server over SCION, or records it locally",
    args_conflicts_with_subcommands = true,
    subcommand_negates_reqs = true
)]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,

    #[command(flatten)]
    send: Option<SendArgs>,
}

#[derive(Debug, clap::Subcommand)]
enum Command {
    /// Read the meter on an interval and append each reading to a local SQLite file.
    Record(RecordArgs),

    /// Print stored readings from the SQLite file.
    Show(ShowArgs),
}

/// Arguments for the default "send one message over SCION" behaviour.
#[derive(Debug, clap::Args)]
struct SendArgs {
    /// URL of the endhost API this client attaches to, for example
    /// `http://192.168.1.42:31000`.
    #[arg(long)]
    endhost_api: Url,

    /// SCION address of the server, for example `[2-ff00:0:212,192.168.1.42]:31337`.
    #[arg(long)]
    server: ScionSocketIpAddr,

    /// Path to POST to on the server.
    #[arg(long, default_value = "/edh/v1/hello")]
    path: String,

    /// Message to send.
    #[arg(long, default_value = "Hello from Energy Data Hackdays 2026")]
    message: String,
}

/// Arguments for `record`.
#[derive(Debug, clap::Args)]
struct RecordArgs {
    /// IP address of the UMG 605-PRO meter.
    #[arg(long)]
    meter_ip: IpAddr,

    /// Modbus TCP port of the meter.
    #[arg(long, default_value_t = DEFAULT_MODBUS_PORT)]
    meter_port: u16,

    /// Modbus unit id configured on the meter.
    #[arg(long, default_value_t = DEFAULT_MODBUS_UNIT)]
    meter_unit: u8,

    /// SQLite file to append readings to. Created if it does not exist.
    #[arg(long, default_value = "pqmeter.db")]
    db: PathBuf,

    /// Seconds between readings.
    #[arg(long, default_value_t = 1.0)]
    interval: f64,

    /// Timeout in seconds for connecting and for each register read.
    #[arg(long, default_value_t = 5)]
    timeout: u64,
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args = Args::parse();

    match (args.command, args.send) {
        (Some(Command::Record(record_args)), _) => record(record_args).await,
        (Some(Command::Show(show_args)), _) => show(show_args),
        (None, Some(send_args)) => send(send_args).await,
        (None, None) => Err(anyhow::anyhow!(
            "nothing to do: run `record` or `show`, or pass --endhost-api and --server to send. See --help."
        )),
    }
}

/// Sends one message to the server over SCION and prints the answer.
async fn send(args: SendArgs) -> anyhow::Result<()> {
    // The SDK uses rustls for its control plane; pick a crypto backend.
    scion_sdk_utils::rustls::select_ring_crypto_provider();

    // One client per program: it holds the connection pool. Building it does no I/O, the
    // connection is established with the first request.
    let client = Client::new(
        Config::new(args.endhost_api)
            // A dummy token. Normally a client asks the AA (the authentication and
            // authorization service) for a SNAP token; here the AA is left out.
            .with_auth_token(snap_tokens::v0::dummy_snap_token())
            // The server uses a self-signed certificate, so its identity is not verified.
            .with_quic_config(QuicConfig::builder().verify_peer(false).build()),
    );

    let body = serde_json::to_vec(&serde_json::json!({ "message": args.message }))
        .context("encoding the message")?;

    // The URL holds the server name and the port. `target` gives the SCION address the
    // packets go to, so the simulated network needs no DNS.
    let url = format!("https://{SERVER_NAME}:{}{}", args.server.port(), args.path);
    let request = Request::post(&url)
        .header("content-type", "application/json")
        .target(args.server.host())
        .body(body)
        .build()
        .context("building the request")?;

    println!("sending to {}{} ...", args.server, args.path);

    let response = client
        .request(request)
        .await
        .context("sending the request")?;
    let status = response.status();
    let (body, _trailers) = response
        .text(Some(MAX_BODY_SIZE))
        .await
        .context("reading the response")?;

    println!("server answered {status}: {body}");

    client.close().await;

    anyhow::ensure!(status.is_success(), "server answered with {status}");
    Ok(())
}

/// Reads the meter every `interval` seconds and appends each reading to the database.
///
/// A read that fails (a meter blip, a timeout) is logged and skipped; the loop keeps
/// going. Only a failure to connect at startup, or to open the database, stops it.
async fn record(args: RecordArgs) -> anyhow::Result<()> {
    anyhow::ensure!(
        args.interval.is_finite() && args.interval > 0.0,
        "interval must be a positive number of seconds, got {}",
        args.interval
    );
    let period = Duration::from_secs_f64(args.interval);
    let timeout = Duration::from_secs(args.timeout);
    let meter_addr = SocketAddr::new(args.meter_ip, args.meter_port);

    let store = MeterStore::open(&args.db)
        .with_context(|| format!("opening the database at {}", args.db.display()))?;
    let mut client =
        Umg605ProClient::connect_tcp(meter_addr, ModbusUnit(args.meter_unit), timeout)
            .await
            .with_context(|| format!("connecting to the meter at {meter_addr}"))?;

    println!(
        "recording {meter_addr} to {} every {:.2?}",
        args.db.display(),
        period
    );

    let mut ticker = tokio::time::interval(period);
    loop {
        ticker.tick().await;
        match read_l1(&mut client).await {
            Ok(reading) => match store.insert_now(reading) {
                Ok(()) => println!(
                    "stored: {:.2} V, {:.2} A, {:.2} W, {:.2} var, {:.2} deg",
                    reading.voltage_l1,
                    reading.current_l1,
                    reading.real_power_l1,
                    reading.reactive_power_l1,
                    reading.phase_angle_l1
                ),
                Err(e) => eprintln!("warning: could not store reading: {e:#}"),
            },
            Err(e) => eprintln!("warning: could not read the meter: {e}"),
        }
    }
}

/// Reads the five L1 measured values and assembles a [`Reading`].
///
/// `ts_millis` is left at zero; [`MeterStore::insert_now`] stamps it.
async fn read_l1(client: &mut Umg605ProClient) -> Result<Reading, ReadError> {
    Ok(Reading {
        ts_millis: 0,
        voltage_l1: client.voltage_l1().await? as f64,
        current_l1: client.current_l1().await? as f64,
        real_power_l1: client.power_l1_n().await? as f64,
        reactive_power_l1: client.reactive_power_l1().await? as f64,
        phase_angle_l1: client.phase_angle_l1().await? as f64,
    })
}

/// Prints stored readings: `--since-id` for everything after a row id (oldest first),
/// otherwise the most recent `--last` (newest first).
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
    serde_json::json!({
        "id": row.id,
        "ts_millis": row.reading.ts_millis,
        "voltage_l1": row.reading.voltage_l1,
        "current_l1": row.reading.current_l1,
        "real_power_l1": row.reading.real_power_l1,
        "reactive_power_l1": row.reading.reactive_power_l1,
        "phase_angle_l1": row.reading.phase_angle_l1,
    })
    .to_string()
}

fn print_table(rows: &[StoredReading]) {
    println!(
        "{:>8}  {:<19}  {:>8}  {:>7}  {:>9}  {:>9}  {:>7}",
        "id", "time (UTC)", "V L1", "A L1", "W L1", "var L1", "deg L1"
    );
    for row in rows {
        let r = &row.reading;
        let time = chrono::DateTime::from_timestamp_millis(r.ts_millis)
            .map(|dt| dt.format("%Y-%m-%d %H:%M:%S").to_string())
            .unwrap_or_else(|| r.ts_millis.to_string());
        println!(
            "{:>8}  {:<19}  {:>8.2}  {:>7.2}  {:>9.2}  {:>9.2}  {:>7.2}",
            row.id,
            time,
            r.voltage_l1,
            r.current_l1,
            r.real_power_l1,
            r.reactive_power_l1,
            r.phase_angle_l1
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_invocation_is_the_send_command() {
        let args = Args::try_parse_from([
            "pq-meter-client",
            "--endhost-api",
            "http://127.0.0.1:31000/",
            "--server",
            "[2-ff00:0:212,127.0.0.1]:59218",
        ])
        .unwrap();

        assert!(args.command.is_none());
        let send = args.send.expect("send args parsed");
        assert_eq!(send.message, "Hello from Energy Data Hackdays 2026");
        assert_eq!(send.path, "/edh/v1/hello");
    }

    #[test]
    fn record_needs_only_the_meter_ip() {
        let args =
            Args::try_parse_from(["pq-meter-client", "record", "--meter-ip", "192.168.1.50"])
                .unwrap();

        let Some(Command::Record(rec)) = args.command else {
            panic!("expected the record command, got {:?}", args.command);
        };
        assert_eq!(rec.meter_ip.to_string(), "192.168.1.50");
        assert_eq!(rec.meter_port, DEFAULT_MODBUS_PORT);
        assert_eq!(rec.db, PathBuf::from("pqmeter.db"));
        assert_eq!(rec.interval, 1.0);
    }

    #[test]
    fn record_does_not_require_the_scion_addresses() {
        // subcommand_negates_reqs: `record` parses without --endhost-api / --server.
        assert!(
            Args::try_parse_from(["pq-meter-client", "record", "--meter-ip", "10.0.0.1"]).is_ok()
        );
    }

    #[test]
    fn record_rejects_mixing_in_the_send_flags() {
        // args_conflicts_with_subcommands: can't do both at once.
        assert!(
            Args::try_parse_from([
                "pq-meter-client",
                "--server",
                "[2-ff00:0:212,127.0.0.1]:1",
                "record",
                "--meter-ip",
                "10.0.0.1",
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
            Args::try_parse_from([
                "pq-meter-client",
                "show",
                "--last",
                "5",
                "--since-id",
                "10"
            ])
            .is_err()
        );
    }

    #[test]
    fn select_rows_follows_since_id_when_set() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pqmeter.db");
        let store = MeterStore::open(&path).unwrap();
        for _ in 0..5 {
            store.insert_now(sample()).unwrap();
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
            store.insert_now(sample()).unwrap();
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

    fn sample() -> Reading {
        Reading {
            ts_millis: 0,
            voltage_l1: 230.0,
            current_l1: 1.8,
            real_power_l1: 414.0,
            reactive_power_l1: 12.0,
            phase_angle_l1: 3.0,
        }
    }
}
