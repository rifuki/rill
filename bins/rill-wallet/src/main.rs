//! The local signer, distributed as a single binary.
//!
//! It holds the key, trusts no bytes from the server without independent inspection, and signs
//! only an envelope that has passed every state transition in `rill-policy`.
//!
//! Run with `--status` it reports readiness and exits — the first thing to reach for when a
//! connector is not working. Otherwise it speaks MCP over stdio, which is how an agent drives it.

use std::io::{stdin, stdout, BufReader};

use rill_wallet::keystore::Keystore;
use rill_wallet::stdio::{serve, WalletContext};

fn main() {
    let keystore = match Keystore::from_env() {
        Ok(store) => Some(store),
        Err(e) => {
            // stderr, always. stdout is the protocol wire, and one stray line on it corrupts the
            // stream with no indication of where the corruption came from.
            eprintln!("rill-wallet: {e}");
            None
        }
    };
    let network = std::env::var("SUI_NETWORK").unwrap_or_else(|_| "testnet".into());
    let mainnet_allowed = std::env::var("RILL_ALLOW_MAINNET").as_deref() == Ok("true");

    if std::env::args().any(|a| a == "--status") {
        match &keystore {
            Some(store) => {
                println!("rill-wallet");
                println!("  status : ready");
                println!("  address: {}", store.address());
                println!("  network: {network}");
                println!(
                    "  mainnet signing: {}",
                    if mainnet_allowed { "allowed" } else { "off" }
                );
            }
            None => {
                println!("rill-wallet");
                println!("  status : not ready");
                std::process::exit(1);
            }
        }
        return;
    }

    match &keystore {
        Some(store) => eprintln!("rill-wallet ready — {network} ({})", store.address()),
        None => eprintln!("rill-wallet started with no key; only read-only tools will answer"),
    }

    let mut context = WalletContext::new(keystore, network, mainnet_allowed);
    if let Err(e) = serve(&mut context, BufReader::new(stdin()), stdout()) {
        eprintln!("rill-wallet: transport stopped: {e}");
        std::process::exit(1);
    }
}
