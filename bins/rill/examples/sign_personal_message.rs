//! Sign a server's sign-in challenge with a key from the local Sui keystore.
//!
//! `sui keytool sign` parses its input as `TransactionData`, so it cannot sign a personal message.
//! This reads the message from stdin and prints the signature as base64, which is what
//! `/oauth/wallet-token` takes. The key never leaves this process.
//!
//! ```sh
//! printf '%s' "$MESSAGE" | cargo run -q -p rill --example sign_personal_message -- <address>
//! ```

use std::io::Read;

fn main() {
    let address = std::env::args()
        .nth(1)
        .expect("usage: sign_personal_message <address>")
        .parse()
        .expect("an address");
    let mut message = Vec::new();
    std::io::stdin()
        .read_to_end(&mut message)
        .expect("reading the message");
    let keystore = rill_cli::keystore::Keystore::load_for(address).expect("a key for that address");
    let signature = keystore
        .sign_personal_message(&message)
        .expect("signing the message");
    println!("{}", signature.to_base64());
}
