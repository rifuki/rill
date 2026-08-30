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

mod envelope;
mod routes;
mod state;

use std::net::SocketAddr;

use crate::state::AppState;

#[tokio::main]
async fn main() {
    let config = state::Config::from_env();
    if let Err(reason) = config.boot_check() {
        eprintln!("{reason}");
        std::process::exit(1);
    }

    let port = config.port;
    let state = AppState::new(config);
    let app = routes::router(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
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
