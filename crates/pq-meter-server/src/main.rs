//! Server side of the Energy Data Hackdays gateway challenge.
//!
//! This binary does two things:
//!
//! 1. It starts a simulated SCION network (PocketSCION) with two ASes, see [`network`].
//! 2. It runs an HTTP/3 server inside one of those ASes, see [`api`].
//!
//! Run it on the laptop; run `pq-meter-client` on the gateway.

mod api;
mod network;
mod storage;

use std::{net::IpAddr, path::PathBuf, sync::Arc};

use anyhow::Context;
use clap::Parser;
use pocketscion::util::dev_auth_token;
use scion_quic::socket::GenericScionUdpSocket;
use scion_stack::stack::ScionStackBuilder;

/// Command line arguments.
#[derive(Debug, Parser)]
#[command(
    version,
    about = "Simulated SCION network with an HTTP/3 server for meter data"
)]
struct Args {
    /// IP address the simulated SCION network exposes its interfaces on.
    ///
    /// The default is only reachable on this machine. To let the gateway connect, pass the
    /// address of the interface it can reach, for example the WLAN address of this laptop.
    #[arg(long, default_value = "127.0.0.1")]
    bind_ip: IpAddr,

    /// Path the HTTP/3 server accepts POST requests on.
    #[arg(long, default_value = api::DEFAULT_PATH)]
    path: String,

    /// SQLite file received meter data is persisted to. Created if it does
    /// not exist; kept (not truncated) across restarts, since it is the
    /// durable, queryable history this project's Grafana dashboards read
    /// from directly — see `crates/pq-meter-server/TODO.md`.
    #[arg(long, default_value = "data/pqmeter.db")]
    db: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args = Args::parse();

    // The SDK uses rustls for its control plane; pick a crypto backend.
    scion_sdk_utils::rustls::select_ring_crypto_provider();

    if let Some(parent) = args.db.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating the database directory {}", parent.display()))?;
    }

    let network = network::start(args.bind_ip).await?;

    // Attach a SCION stack to the server's AS and open a socket on it. The SNAP assigns the
    // address, so we can only print it once the socket exists.
    let stack = ScionStackBuilder::new()
        .with_endhost_api(network.server_endhost_api.clone())
        .with_auth_token(dev_auth_token())
        .build()
        .await
        .context("building the SCION stack of the server")?;
    let socket = stack
        .bind(None)
        .await
        .context("opening a SCION socket for the server")?;
    let server_address = socket.local_addr();

    println!("SCION network is up");
    println!("  gateway endhost API: {}", network.gateway_endhost_api);
    println!("  HTTP/3 server:       {server_address}");
    println!(
        "  accepting CONNECT tunnels (pulling data every {}s)",
        api::PULL_INTERVAL.as_secs()
    );
    println!("  accepting POST on:   {}", args.path);
    println!("  database:            {}", args.db.display());
    println!();
    println!("Start the client with:");
    // The address is quoted because a shell would otherwise read the square brackets as a
    // file name pattern.
    println!(
        "  pq-meter-client --endhost-api {} --server '{server_address}'",
        network.gateway_endhost_api
    );
    println!();

    api::serve(
        Arc::new(socket) as Arc<dyn GenericScionUdpSocket>,
        &args.path,
        &args.db,
    )
    .await
}
