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

/// The shared `Version` object [`TESTNET_AGENT_WALLET`] gates itself on.
///
/// Every call into the package takes it, so every command and every tool needs the id. It was
/// written out in `main.rs` and again in `stdio.rs`, which is two places for one deployment fact:
/// a redeploy that updated one of them would leave the other building transactions against a
/// version object the package no longer accepts, and the failure arrives as a Move abort.
pub const TESTNET_AGENT_WALLET_VERSION: &str =
    "0xd4f88a6dc271f923f0e55dd96eb8f8762ed4d45199c6719ae92365694478fd65";

/// The previous deployment, kept named so that finding it in a config is recognition rather than
/// research. It exposes `spend()` and none of the hot-potato sequence.
pub const TESTNET_AGENT_WALLET_SUPERSEDED: &str =
    "0xd9265581b6b930f5fd27d9ec98e67b48f876f5de7bd25155639d808e9da636da";

/// The `rill_guard` package on testnet, from its own `Published.toml`.
pub const TESTNET_RILL_GUARD: &str =
    "0xadec99557cf7771bce94737fdd3ea0bcc989d81e0860f3e69af55433dae8c034";

/// Cetus's `integrate` package on testnet, which is where `router::swap` lives.
///
/// Defaulted rather than asked for, like the wallet package above. An agent given a pool id and
/// nothing else cannot invent this, and a caller that had to supply it would be a caller who could
/// supply the wrong one: a swap routed through a package that is not Cetus's router fails in a way
/// that names neither the package nor the pool. Verified on chain as a package, immutable.
pub const TESTNET_CETUS_INTEGRATE: &str =
    "0xab2d58dd28ff0dc19b18ab2c634397b785a38c342a8f5065ade5f53f9dbffa1c";

/// Cetus's `GlobalConfig` on testnet, which every swap reads.
///
/// Shared, and owned by the CLMM package rather than by `integrate`: its type is
/// `0x5372d555…::config::GlobalConfig`, and `0x5372d555…` is also the package in every pool's own
/// type. That is the check worth making if this ever has to be replaced, because a config from a
/// different CLMM deployment parses as an address and aborts on use.
pub const TESTNET_CETUS_GLOBAL_CONFIG: &str =
    "0xc6273f844b4bc258952c4e477697aa12c918c8e08106fac6b934811298c9820a";

/// Haedal's package on testnet, the latest version and so the call target.
///
/// Not the type-defining package: the `Staking` object's type and haSUI's are both defined at
/// `0x771b0ab9…`, the original. Calling the original rather than the latest runs code a later
/// upgrade replaced, so the two are named separately. `interface::request_stake` was confirmed on
/// this one by reading the deployed package.
pub const TESTNET_HAEDAL_PACKAGE: &str =
    "0x0a6ff2b974e08b65649d334c38db5ca046b78b4a5d892087740b9cdb3eb08e47";

/// Haedal's shared `Staking` object on testnet. Read live: not paused, version 6.
pub const TESTNET_HAEDAL_STAKING: &str =
    "0xb399662ac5d3973256a1e8629a913336449a2baa16847502ce6bdbf4a0003f07";

/// The haSUI coin type on testnet, so a report can say what arrived.
pub const TESTNET_HASUI_TYPE: &str =
    "0x771b0ab909f629d1b8ef68a62ba8e2074d8726804ac6b7e91b23cdc855117683::hasui::HASUI";

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
