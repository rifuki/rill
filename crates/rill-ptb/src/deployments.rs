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

/// Marks a mainnet id that exists only once the coordinator has published the agent_wallet v3
/// upgrade. Deliberately not an address, so nothing can build a transaction against it.
pub const UNPUBLISHED: &str = "unpublished";

/// Whether an id is still [`UNPUBLISHED`]. `const` so the call target below is chosen at compile
/// time rather than by whichever code path happens to read it.
pub const fn is_unpublished(id: &str) -> bool {
    let (a, b) = (id.as_bytes(), UNPUBLISHED.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

/// `next` once it is published, else `current`.
pub const fn latest<'a>(next: &'a str, current: &'a str) -> &'a str {
    if is_unpublished(next) {
        current
    } else {
        next
    }
}

/// The `agent_wallet` v2 `published-at` on mainnet (upgrade `8FMjfPWQ…`, `deploy/mainnet-v2.json`).
/// The original publication was 2026-10-03 in `9SFxUDKGNKGwKcmKHZ6dQWj91WeYywUqMH2BPTZonZSG`, and
/// its id `0xb8b95f9b…` stays the type origin of `AgentWallet`, `AgentCap` and `SpendRequest`.
///
/// Authoritative until the v3 `migrate` runs. After it, every agent call into this package aborts
/// `version` code 0; owner exits on it keep working because they are not version-gated.
pub const MAINNET_AGENT_WALLET_V2: &str =
    "0x1bad7dbb6084162202817375332addc489e158caac024f24b9b4c9440d180b7a";

/// The `agent_wallet` v3 `published-at` on mainnet: the recipient allowlist.
///
/// Filled by the coordinator from the upgrade receipt, never guessed. The release workflow runs
/// `deployments::tests::mainnet_v3_ids_are_filled`, so a tag cannot ship with the placeholder.
pub const MAINNET_AGENT_WALLET_V3: &str = UNPUBLISHED;

/// The `agent_wallet` package the signer calls on mainnet.
///
/// Compiled in so that a signer launched by an MCP config needs no environment to find it. It
/// follows [`MAINNET_AGENT_WALLET_V3`] as soon as that is filled, and v2 until then, because before
/// `migrate` the chain accepts only v2 and after it only v3: there is no build that serves both.
pub const MAINNET_AGENT_WALLET: &str = latest(MAINNET_AGENT_WALLET_V3, MAINNET_AGENT_WALLET_V2);

/// The `cetus_adapter` package published against agent_wallet v2 (`deploy/mainnet-v2.json`).
///
/// Its `execute_*` and `configure_*` call v2's `confirm_protected`/`configure_protected`, which
/// check the `Version`, so after the v3 `migrate` every protected swap through it aborts `version`
/// code 0 until the adapter is upgraded and relinked.
pub const MAINNET_CETUS_ADAPTER_V1: &str =
    "0x119d1a38177734b18c1d51895a507a184a619fb5fd8226d37bb94859deb0211f";

/// The `cetus_adapter` `published-at` relinked against agent_wallet v3, filled from its upgrade
/// receipt. The server reads its adapter from `RILL_CETUS_ADAPTER_PACKAGE_ID`, so that deployment
/// setting has to move to this id in the same window.
pub const MAINNET_CETUS_ADAPTER: &str = UNPUBLISHED;

/// The shared `Version` object [`MAINNET_AGENT_WALLET`] gates itself on.
pub const MAINNET_AGENT_WALLET_VERSION: &str =
    "0x86430a8195900cd52d607a81993b6125a51628a9db93e10a1afedd2c5a95ba94";

/// The `rill_guard` package on mainnet, published in `B1VQsDBPNtWE3bShPCNWoRpprB2RGexAF3RbLwx8nWPJ`.
pub const MAINNET_RILL_GUARD: &str =
    "0xfa208560e94310fa66c79df8b9aa1a33b5e3601781906833472ef233388e1e43";

/// Cetus's `integrate` package on mainnet, at its latest version, which is the call target.
///
/// Not the original id `0x996c4d94…`. Cetus checks its own package version on every swap
/// (`config::checked_package_version`), so a call routed through an older `integrate` aborts with
/// code 10 before touching the pool. The original id was hardcoded here until a mainnet quote
/// aborted exactly that way on 2026-10-03; version 17 (`0x9ee0534b…`) simulated cleanly against
/// the SUI/USDC pool the same day. Cetus upgrades again, so when a swap aborts in
/// `checked_package_version`, re-read `packageVersions` for `0x996c4d94…` and update this.
pub const MAINNET_CETUS_INTEGRATE: &str =
    "0x9ee0534b703b14457aee7698ea3a07a5080f954ed2b7e9910fbb7a55506824fa";

/// Cetus's `GlobalConfig` on mainnet, as Cetus's own CLMM integration guide lists it.
pub const MAINNET_CETUS_GLOBAL_CONFIG: &str =
    "0xdaa46292632c3c4d8f31f23ea0f9b36a28ff3677e9684980e4438403a67a3d8f";

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

/// Defining package for DeepBook's BalanceManager on testnet, separate from its upgraded call
/// package in `registry`. The ignored Studio schema probe verifies this against a live object.
pub const TESTNET_DEEPBOOK_MANAGER_TYPE_PACKAGE: &str =
    "0xfb28c4cbc6865bd1c897d26aecbe1f8792d1509a20ffec692c800660cbec6982";

/// TradeCap was introduced alongside BalanceManager and retains the same defining package.
pub const TESTNET_DEEPBOOK_TRADE_CAP_TYPE_PACKAGE: &str = TESTNET_DEEPBOOK_MANAGER_TYPE_PACKAGE;

/// DepositCap was introduced in a later upgrade and has its own defining package. Deriving all
/// type identities from a package's original id would incorrectly reject this live capability.
pub const TESTNET_DEEPBOOK_DEPOSIT_CAP_TYPE_PACKAGE: &str =
    "0x984757fc7c0e6dd5f15c2c66e881dd6e5aca98b725f3dbd83c445e057ebb790a";

/// Resolve public deployment IDs: the ones given, else the network's own published pair.
///
/// Never a testnet id on mainnet, given or defaulted. Half a pair is refused rather than completed,
/// because a package from one deployment and a `Version` from another fails on first use.
pub fn wallet_deployment(
    network: rill_core::envelope::Network,
    package: Option<&str>,
    version: Option<&str>,
) -> Result<(sui_sdk_types::Address, sui_sdk_types::Address), String> {
    use rill_core::envelope::Network;
    if package.is_some() != version.is_some() {
        return Err(
            "set both AGENT_WALLET_PACKAGE_ID and AGENT_WALLET_VERSION_ID, or neither".into(),
        );
    }
    let (default_package, default_version) = match network {
        Network::Mainnet => (MAINNET_AGENT_WALLET, MAINNET_AGENT_WALLET_VERSION),
        Network::Testnet => (TESTNET_AGENT_WALLET, TESTNET_AGENT_WALLET_VERSION),
    };
    let package = package
        .unwrap_or(default_package)
        .parse::<sui_sdk_types::Address>()
        .map_err(|_| "wallet package is not a Sui address")?;
    let version = version
        .unwrap_or(default_version)
        .parse::<sui_sdk_types::Address>()
        .map_err(|_| "wallet Version object is not a Sui address")?;
    if network == Network::Mainnet
        && (package.to_string() == TESTNET_AGENT_WALLET
            || version.to_string() == TESTNET_AGENT_WALLET_VERSION)
    {
        return Err("mainnet cannot use testnet wallet deployment IDs".into());
    }
    Ok((package, version))
}

/// The `rill_guard` package for a network, unless one is given.
pub fn guard_package(network: rill_core::envelope::Network, given: Option<&str>) -> String {
    given.map(str::to_owned).unwrap_or_else(|| {
        match network {
            rill_core::envelope::Network::Mainnet => MAINNET_RILL_GUARD,
            rill_core::envelope::Network::Testnet => TESTNET_RILL_GUARD,
        }
        .to_owned()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The release gate. Ignored in ordinary runs because the ids exist only after the coordinator
    /// publishes; the release workflow runs it with `--ignored`, so no tag can ship a signer that
    /// still targets v2 bytecode after `migrate` retires it.
    #[test]
    #[ignore = "release gate: fails until the agent_wallet v3 upgrade ids are filled in"]
    fn mainnet_v3_ids_are_filled() {
        for (name, id) in [
            ("MAINNET_AGENT_WALLET_V3", MAINNET_AGENT_WALLET_V3),
            ("MAINNET_CETUS_ADAPTER", MAINNET_CETUS_ADAPTER),
        ] {
            assert!(
                !is_unpublished(id),
                "{name} is still the placeholder; fill it from the mainnet upgrade receipt"
            );
            id.parse::<sui_sdk_types::Address>()
                .unwrap_or_else(|_| panic!("{name} is not an address: {id}"));
        }
        assert_eq!(MAINNET_AGENT_WALLET, MAINNET_AGENT_WALLET_V3);
        assert_ne!(MAINNET_CETUS_ADAPTER, MAINNET_CETUS_ADAPTER_V1);
    }

    /// Unfilled, mainnet keeps calling v2, which is what the chain accepts until `migrate`. Filled,
    /// one edit moves every default, so there is no second constant to forget.
    #[test]
    fn the_mainnet_call_target_follows_v3_once_it_is_filled() {
        assert_eq!(
            latest(UNPUBLISHED, MAINNET_AGENT_WALLET_V2),
            MAINNET_AGENT_WALLET_V2
        );
        assert_eq!(latest("0xabc", MAINNET_AGENT_WALLET_V2), "0xabc");
        assert!(is_unpublished(UNPUBLISHED));
        assert!(UNPUBLISHED.parse::<sui_sdk_types::Address>().is_err());
        if is_unpublished(MAINNET_AGENT_WALLET_V3) {
            assert_eq!(MAINNET_AGENT_WALLET, MAINNET_AGENT_WALLET_V2);
        }
    }

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
