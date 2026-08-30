//! Haedal liquid staking: SUI in, haSUI out.
//!
//! # The minimum is checked here, not discovered on chain
//!
//! Haedal aborts below one SUI. Building a transaction that is certain to abort wastes gas and
//! produces a failure whose cause lives in a Move abort code rather than anywhere a user is
//! looking, so the amount is refused before a single command is emitted.

use sui_sdk_types::{Address, Identifier};
use sui_transaction_builder::{Argument, Function, TransactionBuilder};

use crate::shared::{SharedObjects, UnknownSharedVersion};

/// Haedal's floor: one SUI, in mist. Below this, `request_stake` aborts with code 4.
pub const MIN_STAKE_MIST: u64 = 1_000_000_000;

/// Sui's shared system state object, which staking reads validator information from.
pub const SUI_SYSTEM_STATE_ID: &str = "0x5";

#[derive(Clone)]
pub struct Stake {
    /// The published Haedal package.
    pub package_id: Address,
    /// Haedal's shared staking object.
    pub staking_object_id: Address,
    /// Which validator to delegate to.
    pub validator: Address,
    /// How much SUI is being staked, in mist. Used for the floor check; the coin carries the value.
    pub amount_mist: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HaedalError {
    /// A shared object was referenced before its initial version was known.
    UnknownShared(UnknownSharedVersion),
    /// Below Haedal's own minimum, refused before anything is built.
    BelowMinimum {
        amount: u64,
    },
    BadIdentifier(String),
}

impl std::fmt::Display for HaedalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownShared(e) => write!(f, "{e}"),
            Self::BelowMinimum { amount } => write!(
                f,
                "staking {amount} mist is below Haedal's minimum of {MIN_STAKE_MIST} (1 SUI); this \
                 would abort on chain, so it is refused before any gas is spent"
            ),
            Self::BadIdentifier(s) => write!(f, "\"{s}\" is not a valid Move identifier"),
        }
    }
}

impl std::error::Error for HaedalError {}

impl From<UnknownSharedVersion> for HaedalError {
    fn from(e: UnknownSharedVersion) -> Self {
        Self::UnknownShared(e)
    }
}

fn ident(s: &str) -> Result<Identifier, HaedalError> {
    Identifier::new(s).map_err(|_| HaedalError::BadIdentifier(s.to_owned()))
}

/// Emit `request_stake`.
///
/// Consumes the SUI coin and produces no chainable output — the resulting haSUI goes to the
/// sender, so this is a terminal step in a flow rather than something another node can read from.
pub fn request_stake(
    tx: &mut TransactionBuilder,
    stake: &Stake,
    sui_coin: Argument,
    // Initial shared versions read from the chain; a missing one refuses the build.
    shared: &SharedObjects,
) -> Result<(), HaedalError> {
    if stake.amount_mist < MIN_STAKE_MIST {
        return Err(HaedalError::BelowMinimum {
            amount: stake.amount_mist,
        });
    }

    let system_state = tx.object(shared.input(
        SUI_SYSTEM_STATE_ID.parse().expect("0x5 is a valid address"),
        true,
    )?);
    let staking = tx.object(shared.input(stake.staking_object_id, true)?);
    let validator = tx.pure(&stake.validator);

    tx.move_call(
        Function::new(stake.package_id, ident("staking")?, ident("request_stake")?),
        vec![system_state, staking, sui_coin, validator],
    );
    Ok(())
}

/// The target a stake emits, for the signer's pinned sequence.
pub fn expected_stake_targets(package_id: Address) -> Vec<String> {
    vec![format!("{package_id}::staking::request_stake")]
}
