//! Which deployed `agent_wallet` package is the real one.
//!
//! # The reference repo names two, and only one of them works
//!
//! Its README and pitch deck name `0xd9265581…a636da`. Its `Published.toml` and `.env.example`
//! name `0xb02f39d6…563740`. Nothing in the repo says which is current, and its documentation
//! describes a `spend()` entry point that the Move source no longer contains — which reads like
//! stale prose until you ask the chain.
//!
//! Asked on testnet (`tests/package_probe.rs` in `rill-chain`, reproducible):
//!
//! ```text
//! 0xd9265581…a636da   request_spend absent   confirm_spend absent   spend present
//! 0xb02f39d6…563740   request_spend present  confirm_spend present  spend absent
//! ```
//!
//! So the two addresses are two *generations*. The README points at the old one, and the docs
//! describing `spend()` are not stale prose at all — they correctly describe the package the README
//! names. The drift is in which deployment is being pointed at, not in the words.
//!
//! # The demo wallet holds capabilities for the old one
//!
//! The funded testnet sender's three `AgentCap` objects are all typed
//! `0xd9265581…::agent_wallet::AgentCap`. A capability minted by one package cannot authorise a
//! call in another — the type does not match — so those caps cannot drive [`TESTNET_AGENT_WALLET`].
//! An end-to-end submission needs a cap minted from the current package first. This is a fact about
//! the deployment, not something the code can work around, and it is recorded here so it is found
//! before a demo rather than during one.

/// The current `agent_wallet` package on testnet: the one with the hot-potato sequence.
///
/// Verified on chain rather than taken from a document. See the module note.
pub const TESTNET_AGENT_WALLET: &str =
    "0xb02f39d682d0471344b1cc264f6f29d625280b9e73560d5beee3db3090563740";

/// The previous deployment, kept named so that finding it in a config is recognition rather than
/// research. It exposes `spend()` and none of the hot-potato sequence.
pub const TESTNET_AGENT_WALLET_SUPERSEDED: &str =
    "0xd9265581b6b930f5fd27d9ec98e67b48f876f5de7bd25155639d808e9da636da";

/// The `rill_guard` package on testnet, from its own `Published.toml`.
pub const TESTNET_RILL_GUARD: &str =
    "0xadec99557cf7771bce94737fdd3ea0bcc989d81e0860f3e69af55433dae8c034";

/// Whether an address is the superseded deployment, so a caller can say so plainly instead of
/// letting a Move abort explain it.
pub fn is_superseded(package_id: &str) -> bool {
    package_id.eq_ignore_ascii_case(TESTNET_AGENT_WALLET_SUPERSEDED)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_two_generations_are_not_the_same_address() {
        assert_ne!(TESTNET_AGENT_WALLET, TESTNET_AGENT_WALLET_SUPERSEDED);
    }

    #[test]
    fn the_superseded_deployment_is_recognised() {
        assert!(is_superseded(TESTNET_AGENT_WALLET_SUPERSEDED));
        assert!(is_superseded(
            &TESTNET_AGENT_WALLET_SUPERSEDED
                .to_uppercase()
                .replace("0X", "0x")
        ));
        assert!(!is_superseded(TESTNET_AGENT_WALLET));
    }
}
