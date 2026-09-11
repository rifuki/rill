//! Rill's keyless server: REST, the MCP endpoints, and the OAuth authorization server.
//!
//! It builds and simulates transactions and holds no key. Nothing in this binary can sign.
//!
//! # Mount points are load-bearing
//!
//! `/.well-known/*` and `/mcp` sit at the **origin root**, not under `/api`. RFC 8414 and RFC 9728
//! define those discovery paths as origin-relative and every MCP client probes exactly there;
//! serving them one prefix deeper would be invisible to all of them, and the failure would look
//! like "the connector just doesn't work" with nothing in the logs to explain it.

use std::net::SocketAddr;

use rill_server::routes;
use rill_server::state::{AppState, Config};

#[tokio::main]
async fn main() {
    let config = Config::from_env();
    if let Err(reason) = config.boot_check() {
        eprintln!("{reason}");
        std::process::exit(1);
    }

    let port = config.port;
    // Parsed before the router is built so a bad address fails before anything else is set up.
    // `boot_check` has already decided whether this address is allowed to be a wide one.
    let host: std::net::IpAddr = match config.bind_address.trim() {
        "localhost" => std::net::IpAddr::from([127, 0, 0, 1]),
        other => match other.parse() {
            Ok(ip) => ip,
            Err(_) => {
                eprintln!(
                    "BIND_ADDRESS is {other:?}, which is not an IP address. Use 127.0.0.1 to keep \
                     this on one machine, or 0.0.0.0 for every interface."
                );
                std::process::exit(1);
            }
        },
    };
    let app = routes::router(AppState::new(config));

    let addr = SocketAddr::new(host, port);
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("could not bind {addr}: {e}");
            std::process::exit(1);
        }
    };
    eprintln!("rill-server listening on {addr}");
    if let Err(e) = axum::serve(listener, app).await {
        eprintln!("server stopped: {e}");
        std::process::exit(1);
    }
}
