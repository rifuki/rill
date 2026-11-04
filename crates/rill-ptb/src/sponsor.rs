//! The wallet's owner as gas sponsor of an agent's spend.
//!
//! The on-chain `step_up` rule aborts a transaction whose released total is above the owner's
//! threshold unless its gas sponsor is the owner. Only the gas owner can produce the sponsor
//! signature, so such a transaction lands only with the owner's own signature over its exact bytes:
//! the agent stays the sender and signs as sender, and the owner signs as sponsor.
//!
//! # Nothing may touch the gas coin
//!
//! In a sponsored transaction `GasCoin` is the owner's SUI. A `SplitCoins(GasCoin)` there would
//! hand the agent the owner's own funds, outside the wallet and outside every rule, inside a
//! transaction the owner signed for a different reason. So a transaction that uses the gas coin
//! anywhere is refused here, and the signer and Studio refuse it again from the bytes.

use sui_sdk_types::{Address, Argument, Command, ObjectReference, Transaction, TransactionKind};

/// The most gas an owner is asked to sponsor for one transaction: 0.1 SUI, the budget every Studio
/// action is compiled with. Studio refuses to ask the owner to sign above it, so a builder that
/// raised it would be refused there rather than charge the owner more.
pub const MAX_SPONSORED_GAS_BUDGET_MIST: u64 = 100_000_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SponsorError {
    /// The budget is above [`MAX_SPONSORED_GAS_BUDGET_MIST`].
    BudgetTooHigh { budget: u64 },
    /// A command reads or moves `GasCoin`, which would be the owner's.
    TouchesGasCoin,
    /// The owner holds no SUI to pay with.
    NoGasCoins,
    /// The sponsor is the sender, which Sui treats as no sponsor at all.
    SponsorIsSender,
    /// Not a programmable transaction.
    NotProgrammable,
}

impl std::fmt::Display for SponsorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TouchesGasCoin => write!(
                f,
                "the transaction uses its gas coin, which the owner would pay with, so it cannot \
                 be sponsored by the owner"
            ),
            Self::NoGasCoins => write!(f, "the owner holds no SUI coin to pay the gas with"),
            Self::BudgetTooHigh { budget } => write!(
                f,
                "a gas budget of {budget} is above the {MAX_SPONSORED_GAS_BUDGET_MIST} an owner is \
                 asked to sponsor"
            ),
            Self::SponsorIsSender => write!(
                f,
                "the owner is the transaction's sender, so it cannot also be its sponsor: on a \
                 wallet whose agent is its owner, the on-chain step-up rule makes every spend above \
                 its threshold impossible. Remove the rule in Studio, or give the wallet a separate \
                 agent"
            ),
            Self::NotProgrammable => write!(f, "only a programmable transaction can be sponsored"),
        }
    }
}

impl std::error::Error for SponsorError {}

/// Whether any command in `transaction` reads or moves its gas coin.
pub fn touches_gas_coin(transaction: &Transaction) -> Result<bool, SponsorError> {
    let TransactionKind::ProgrammableTransaction(ptb) = &transaction.kind else {
        return Err(SponsorError::NotProgrammable);
    };
    let gas = |arg: &Argument| *arg == Argument::Gas;
    Ok(ptb.commands.iter().any(|command| match command {
        Command::MoveCall(call) => call.arguments.iter().any(gas),
        Command::SplitCoins(split) => gas(&split.coin) || split.amounts.iter().any(gas),
        Command::TransferObjects(transfer) => {
            gas(&transfer.address) || transfer.objects.iter().any(gas)
        }
        Command::MergeCoins(merge) => gas(&merge.coin) || merge.coins_to_merge.iter().any(gas),
        Command::MakeMoveVector(vector) => vector.elements.iter().any(gas),
        // Publish and Upgrade take no arguments that can name the gas coin. A command added
        // upstream later is assumed to touch it, which refuses rather than sponsors.
        Command::Publish(_) | Command::Upgrade(_) => false,
        _ => true,
    }))
}

/// Make `owner` the gas owner of `transaction`, paying with `coins`. The sender is unchanged.
///
/// Refuses, and leaves the transaction as it was, when anything touches the gas coin, when there is
/// nothing to pay with, when `owner` is the sender, or when the budget is above the ceiling.
pub fn sponsor_gas(
    transaction: &mut Transaction,
    owner: Address,
    coins: Vec<ObjectReference>,
) -> Result<(), SponsorError> {
    if touches_gas_coin(transaction)? {
        return Err(SponsorError::TouchesGasCoin);
    }
    if coins.is_empty() {
        return Err(SponsorError::NoGasCoins);
    }
    if owner == transaction.sender {
        return Err(SponsorError::SponsorIsSender);
    }
    if transaction.gas_payment.budget > MAX_SPONSORED_GAS_BUDGET_MIST {
        return Err(SponsorError::BudgetTooHigh {
            budget: transaction.gas_payment.budget,
        });
    }
    transaction.gas_payment.owner = owner;
    transaction.gas_payment.objects = coins;
    Ok(())
}
