//! The on-chain slippage floor.
//!
//! `rill_guard::guard::assert_min_value` takes the coin by immutable reference, so the coin stays
//! usable downstream — the guard sits *in* the flow rather than terminating it.
//!
//! Two refusals live here, and both exist because the alternative is a guard that looks enforced
//! and is not:
//!
//! - A floor of zero is not a floor. Emitting the call anyway would put an assertion in the
//!   transaction that can never fail, which reads to anyone inspecting it as protection.
//! - A floor above zero with no deployed guard package is refused outright rather than skipped.
//!   Silently dropping the only thing standing between a swap and an unbounded loss is the worst
//!   available answer.

use sui_sdk_types::{Address, Identifier};
use sui_transaction_builder::{Argument, Function, TransactionBuilder};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardError {
    /// A floor was requested but no guard package is configured.
    NoGuardPackage {
        min_out: u64,
    },
    BadIdentifier(String),
}

impl std::fmt::Display for GuardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoGuardPackage { min_out } => write!(
                f,
                "a minimum output of {min_out} was required but no rill_guard package is \
                 configured; refusing to build an unguarded transaction"
            ),
            Self::BadIdentifier(s) => write!(f, "\"{s}\" is not a valid Move identifier"),
        }
    }
}

impl std::error::Error for GuardError {}

/// Whether a guard was actually emitted — returned so a caller can report honestly rather than
/// assuming.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardOutcome {
    Enforced,
    /// No floor was asked for, so nothing was emitted and nothing is protected.
    NotRequested,
}

/// Assert a coin's value meets a floor, leaving the coin usable.
///
/// `min_out == 0` emits nothing and says so. A caller that wanted protection and got
/// `NotRequested` has learned something it needs to surface, which is why this is a return value
/// and not a silent no-op.
pub fn assert_min_value(
    tx: &mut TransactionBuilder,
    guard_package: Option<Address>,
    coin: Argument,
    coin_type: &str,
    min_out: u64,
) -> Result<GuardOutcome, GuardError> {
    if min_out == 0 {
        return Ok(GuardOutcome::NotRequested);
    }
    let Some(package) = guard_package else {
        return Err(GuardError::NoGuardPackage { min_out });
    };

    let ident = |s: &str| Identifier::new(s).map_err(|_| GuardError::BadIdentifier(s.to_owned()));
    let type_arg: sui_sdk_types::TypeTag = coin_type
        .parse()
        .map_err(|_| GuardError::BadIdentifier(coin_type.to_owned()))?;

    let min = tx.pure(&min_out);
    tx.move_call(
        Function::new(package, ident("guard")?, ident("assert_min_value")?)
            .with_type_args(vec![type_arg]),
        vec![coin, min],
    );
    Ok(GuardOutcome::Enforced)
}

/// The target a guard emits, for the signer's pinned sequence.
pub fn guard_target(guard_package: Address) -> String {
    format!("{guard_package}::guard::assert_min_value")
}
