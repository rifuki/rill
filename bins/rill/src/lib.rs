//! The local signer, as a library so the key-handling surface is testable and the binary stays thin.
//!
//! Everything security-relevant lives here. `main` reads the environment, reports readiness, and
//! otherwise gets out of the way.

pub mod keystore;
pub mod rules_cmd;
pub mod runset;
pub mod spend_cmd;
pub mod stdio;
pub mod wallet;
