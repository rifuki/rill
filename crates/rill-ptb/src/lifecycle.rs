//! Owner-only wallet operations, including the way out.
//!
//! # The one that has to exist
//!
//! `create_wallet` funds a wallet and hands its capability to an agent in a single call, with an
//! empty policy. If the step that attaches rules then fails permanently — the wrong key, a stale
//! Version, a node that stops answering — the funds are sitting in a wallet nobody has bounded.
//! Until now this repo had no call that got them back.
//!
//! `revoke` is the contract's kill switch: it marks the wallet revoked and returns the whole
//! remaining balance as a coin. Every later `request_spend` aborts `E_REVOKED`. It is deliberately
//! not version-gated, so it keeps working through a package upgrade — which is exactly when you
//! would most want it.
//!
//! # All of these are the owner's, and none of them are the agent's
//!
//! Each asserts `ctx.sender() == wallet.owner`. Signed with the agent's key they abort
//! `E_NOT_OWNER` (1), which the abort table names. None takes the `Version` object: the contract
//! does not gate owner operations on it, on purpose, so an upgrade cannot trap a wallet.

use sui_sdk_types::{Address, Identifier};
use sui_transaction_builder::{Argument, Function, TransactionBuilder};

use crate::shared::{SharedObjects, UnknownSharedVersion};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LifecycleError {
    BadIdentifier(String),
    UnknownShared(UnknownSharedVersion),
    /// An expiry that is not strictly later than the current one aborts `E_EXPIRY_NOT_FORWARD`.
    ExpiryNotForward {
        current: u64,
        requested: u64,
    },
}

impl std::fmt::Display for LifecycleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadIdentifier(s) => write!(f, "\"{s}\" is not a valid Move identifier or type"),
            Self::UnknownShared(e) => write!(f, "{e}"),
            Self::ExpiryNotForward { current, requested } => write!(
                f,
                "an expiry may only move forward: {requested} is not later than {current}. The \
                 contract refuses this so a live wallet's lifetime can never be shortened out from \
                 under its agent."
            ),
        }
    }
}

impl std::error::Error for LifecycleError {}

impl From<UnknownSharedVersion> for LifecycleError {
    fn from(e: UnknownSharedVersion) -> Self {
        Self::UnknownShared(e)
    }
}

fn ident(s: &str) -> Result<Identifier, LifecycleError> {
    Identifier::new(s).map_err(|_| LifecycleError::BadIdentifier(s.to_owned()))
}

fn call(
    tx: &mut TransactionBuilder,
    package_id: Address,
    function: &str,
    coin_type: &str,
    args: Vec<Argument>,
) -> Result<Argument, LifecycleError> {
    let coin: sui_sdk_types::TypeTag = coin_type
        .parse()
        .map_err(|_| LifecycleError::BadIdentifier(coin_type.to_owned()))?;
    Ok(tx.move_call(
        Function::new(package_id, ident("agent_wallet")?, ident(function)?)
            .with_type_args(vec![coin]),
        args,
    ))
}

/// Take everything back and stop the wallet.
///
/// Returns the released coin, which the caller MUST consume. A coin left unconsumed aborts on chain
/// with `UnusedValueWithoutDrop` — and `try_build` does not catch it, so nothing local will. Only
/// the simulation gate stands between forgetting the coin and a revoke that recovers nothing.
pub fn build_revoke(
    tx: &mut TransactionBuilder,
    package_id: Address,
    wallet_id: Address,
    coin_type: &str,
    shared: &SharedObjects,
) -> Result<Argument, LifecycleError> {
    let wallet = tx.object(shared.input(wallet_id, true)?);
    call(tx, package_id, "revoke", coin_type, vec![wallet])
}

/// Add funds to a live wallet. Does not change any rule — a budget ceiling is independent of the
/// balance, so topping up a wallet whose budget is exhausted changes nothing an agent can spend.
pub fn build_top_up(
    tx: &mut TransactionBuilder,
    package_id: Address,
    wallet_id: Address,
    coin_type: &str,
    funds: Argument,
    shared: &SharedObjects,
) -> Result<(), LifecycleError> {
    let wallet = tx.object(shared.input(wallet_id, true)?);
    call(tx, package_id, "top_up", coin_type, vec![wallet, funds])?;
    Ok(())
}

/// Move the capability to a different agent.
///
/// Mints a *new* `AgentCap` and transfers it to the new agent; the old one stops working
/// immediately, because `request_spend` checks the cap's id against the wallet's current `cap_id`.
/// The new id is only knowable from the transaction's effects.
pub fn build_rotate_agent(
    tx: &mut TransactionBuilder,
    package_id: Address,
    wallet_id: Address,
    coin_type: &str,
    new_agent: Address,
    shared: &SharedObjects,
) -> Result<(), LifecycleError> {
    let wallet = tx.object(shared.input(wallet_id, true)?);
    let agent = tx.pure(&new_agent);
    call(
        tx,
        package_id,
        "rotate_agent",
        coin_type,
        vec![wallet, agent],
    )?;
    Ok(())
}

/// Push the expiry later. Refused locally when it is not strictly later, rather than discovered as
/// `E_EXPIRY_NOT_FORWARD` after gas is spent.
pub fn build_extend_expiry(
    tx: &mut TransactionBuilder,
    package_id: Address,
    wallet_id: Address,
    coin_type: &str,
    current_expires_at_ms: u64,
    new_expires_at_ms: u64,
    shared: &SharedObjects,
) -> Result<(), LifecycleError> {
    if new_expires_at_ms <= current_expires_at_ms {
        return Err(LifecycleError::ExpiryNotForward {
            current: current_expires_at_ms,
            requested: new_expires_at_ms,
        });
    }
    let wallet = tx.object(shared.input(wallet_id, true)?);
    let expiry = tx.pure(&new_expires_at_ms);
    call(
        tx,
        package_id,
        "extend_expiry",
        coin_type,
        vec![wallet, expiry],
    )?;
    Ok(())
}

/// The target each lifecycle call emits, for a pinned sequence.
pub fn expected_lifecycle_target(package_id: Address, function: &str) -> String {
    format!("{package_id}::agent_wallet::{function}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use sui_sdk_types::Digest;
    use sui_transaction_builder::ObjectInput;

    fn addr(n: u8) -> Address {
        format!("0x{n:064x}").parse().unwrap()
    }

    fn resolved() -> SharedObjects {
        let mut shared = SharedObjects::new();
        shared.insert(addr(0x20), 400_020);
        shared
    }

    fn builder() -> TransactionBuilder {
        let mut tx = TransactionBuilder::new();
        tx.set_sender(addr(9));
        tx.set_gas_budget(50_000_000);
        tx.set_gas_price(1_000);
        tx.add_gas_objects([ObjectInput::owned(addr(0x0a), 1, Digest::ZERO)]);
        tx
    }

    /// The recovery path, and the coin it returns must be consumed or nothing is recovered.
    #[test]
    fn revoke_returns_a_coin_that_the_caller_consumes() {
        let mut tx = builder();
        let coin = build_revoke(
            &mut tx,
            addr(0xca),
            addr(0x20),
            "0x2::sui::SUI",
            &resolved(),
        )
        .expect("should build");
        let recipient = tx.pure(&addr(9));
        tx.transfer_objects(vec![coin], recipient);
        tx.try_build().expect("valid transaction");
    }

    /// A revoke whose coin is dropped compiles fine and then aborts on chain with
    /// `UnusedValueWithoutDrop`.
    ///
    /// I expected `try_build` to catch it. It does not — the builder tracks command results, not
    /// whether they were consumed, and that is a runtime property. Pinning the real behaviour is
    /// worth more than asserting the behaviour I assumed: it records that this particular mistake
    /// survives every local check and is caught only by simulation, which is why the recovery path
    /// consumes the coin at the call site rather than trusting the compiler to notice.
    #[test]
    fn an_unconsumed_revoke_coin_compiles_and_is_caught_only_on_chain() {
        let mut tx = builder();
        let _coin = build_revoke(
            &mut tx,
            addr(0xca),
            addr(0x20),
            "0x2::sui::SUI",
            &resolved(),
        )
        .expect("should build");
        assert!(
            tx.try_build().is_ok(),
            "the builder does not track consumption; if this ever starts failing, the note above \
             is out of date and the simulation gate is no longer the only thing catching it"
        );
    }

    #[test]
    fn top_up_builds() {
        let mut tx = builder();
        let amount = tx.pure(&1_000_000_000u64);
        let gas = tx.gas();
        let coin = tx
            .split_coins(gas, vec![amount])
            .into_iter()
            .next()
            .unwrap();
        build_top_up(
            &mut tx,
            addr(0xca),
            addr(0x20),
            "0x2::sui::SUI",
            coin,
            &resolved(),
        )
        .expect("should build");
        tx.try_build().expect("valid transaction");
    }

    #[test]
    fn rotate_agent_builds() {
        let mut tx = builder();
        build_rotate_agent(
            &mut tx,
            addr(0xca),
            addr(0x20),
            "0x2::sui::SUI",
            addr(7),
            &resolved(),
        )
        .expect("should build");
        tx.try_build().expect("valid transaction");
    }

    /// The contract refuses a backwards expiry so a live wallet's lifetime cannot be cut short.
    /// Discovering that after gas is spent is worse than being told before.
    #[test]
    fn an_expiry_that_moves_backwards_is_refused_before_gas_is_spent() {
        let mut tx = builder();
        let result = build_extend_expiry(
            &mut tx,
            addr(0xca),
            addr(0x20),
            "0x2::sui::SUI",
            2_000,
            1_999,
            &resolved(),
        );
        assert!(matches!(
            result,
            Err(LifecycleError::ExpiryNotForward { .. })
        ));
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("only move forward"));
    }

    #[test]
    fn an_expiry_equal_to_the_current_one_is_also_refused() {
        let mut tx = builder();
        assert!(build_extend_expiry(
            &mut tx,
            addr(0xca),
            addr(0x20),
            "0x2::sui::SUI",
            2_000,
            2_000,
            &resolved()
        )
        .is_err());
    }

    #[test]
    fn extending_forward_builds() {
        let mut tx = builder();
        build_extend_expiry(
            &mut tx,
            addr(0xca),
            addr(0x20),
            "0x2::sui::SUI",
            2_000,
            3_000,
            &resolved(),
        )
        .expect("should build");
        tx.try_build().expect("valid transaction");
    }

    /// None of these takes the Version object — the contract does not gate owner operations on it,
    /// so an upgrade can never trap a wallet with funds inside.
    #[test]
    fn no_lifecycle_call_needs_the_version_object() {
        let mut shared = SharedObjects::new();
        shared.insert(addr(0x20), 400_020);
        // No version id inserted at all, and every one of these still builds.
        let mut tx = builder();
        let coin = build_revoke(&mut tx, addr(0xca), addr(0x20), "0x2::sui::SUI", &shared)
            .expect("revoke");
        let to = tx.pure(&addr(9));
        tx.transfer_objects(vec![coin], to);
        build_rotate_agent(
            &mut tx,
            addr(0xca),
            addr(0x20),
            "0x2::sui::SUI",
            addr(7),
            &shared,
        )
        .expect("rotate");
        tx.try_build().expect("valid transaction");
    }
}
