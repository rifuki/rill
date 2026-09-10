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
/// nobody downloaded. The cargo target is also built as `rill` for this repository's own use; a
/// stranger downloads `rill-wallet-<platform>`, and the first thing they run should answer with
/// the name they downloaded.
pub const BINARY_NAME: &str = "rill-wallet";

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
