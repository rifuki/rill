//! The local signer, distributed as a single binary.
//!
//! It holds the key, trusts no bytes from the server without independent inspection, and signs
//! only an envelope that has passed every state transition in `rill-policy`.
//!
//! Run with no arguments it reports its own readiness and exits — the first thing to reach for
//! when a connector is not working, and it prints only public values.

use rill_wallet::keystore::Keystore;

fn main() {
    match Keystore::from_env() {
        Ok(store) => {
            println!("rill-wallet");
            println!("  status : ready");
            println!("  address: {}", store.address());
            println!();
            println!("Fund this address before signing anything. It is public — the key behind it");
            println!("was read from the environment and is not written anywhere.");
        }
        Err(e) => {
            println!("rill-wallet");
            println!("  status : not ready");
            println!("  reason : {e}");
            std::process::exit(1);
        }
    }
}
