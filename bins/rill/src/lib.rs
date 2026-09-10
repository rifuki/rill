//! The local signer, as a library so the key-handling surface is testable and the binary stays thin.
//!
//! Everything security-relevant lives here. `main` reads the environment, reports readiness, and
//! otherwise gets out of the way.

/// What the binary calls itself: the name the release ships under, the first line `status`
/// prints, `serverInfo.name` on the MCP handshake, and the string the release workflow's smoke
/// step looks for.
///
/// Declared once because it is a contract in four places, and a rename that reaches three of
/// them produces a release whose smoke step fails, or worse, one whose binary answers to a name
/// nobody downloaded. A stranger downloads `rill-wallet-<platform>`, and the first thing they run
/// should answer with the name they downloaded.
///
/// The value itself now lives in `rill_core::release`, beside the asset filenames that are builds
/// of it and the origin that publishes them, because the server generates install instructions and
/// cannot link this crate: the signer's library is exactly what a keyless builder must not hold.
pub const BINARY_NAME: &str = rill_core::release::WALLET_BINARY;

pub mod init;
pub mod keystore;
pub mod manager_cmd;
pub mod order_cmd;
pub mod revoke_cmd;
pub mod rules_cmd;
pub mod runset;
pub mod spend_cmd;
pub mod stdio;
pub mod verdict;
pub mod wallet;
pub mod wallet_read;
