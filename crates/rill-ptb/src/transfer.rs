//! Sending coins, and splitting them exactly.
//!
//! # The simplest thing here, and the one most easily got wrong
//!
//! A transfer looks like it needs no care: split an amount off, send it. But the amount arrives from
//! a human or an agent as text — "1.5", "0.000000001" — and every intermediate representation
//! between that text and the u64 the chain takes is a chance to lose a unit. The reference
//! implementation lost units in exactly this way, on the DeepBook path, and its tests all passed.
//!
//! So no amount enters here as a number. It enters as a decimal string, is converted once by
//! `rill-core::amounts` against the coin's own decimals, and is a u64 from that point on.
//!
//! # Gas is a coin too
//!
//! For SUI, splitting from the gas coin is not a shortcut — it is the only way to spend an exact
//! amount without first knowing which coin objects the sender holds and whether any single one is
//! large enough. The gas payment is merged before execution, so a split the largest single coin
//! could not cover still succeeds against the whole balance.

use rill_core::amounts::{decimal_to_base_units, AmountError};
use sui_sdk_types::Address;
use sui_transaction_builder::{Argument, TransactionBuilder};

/// One transfer.
#[derive(Debug, Clone)]
pub struct Transfer {
    pub recipient: Address,
    /// Decimal text, as written. Never a float, and never pre-multiplied by the caller.
    pub amount: String,
    /// The coin's decimals — 9 for SUI, 6 for most stablecoins. Wrong here misstates the amount by
    /// orders of magnitude, so it is required rather than defaulted.
    pub decimals: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferError {
    Amount(AmountError),
    ZeroAmount,
    /// Sending to oneself is almost always a mistake, and it costs gas to discover.
    SelfTransfer(Address),
}

impl std::fmt::Display for TransferError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Amount(e) => write!(f, "{e}"),
            Self::ZeroAmount => write!(f, "refusing to build a transfer of zero"),
            Self::SelfTransfer(a) => write!(
                f,
                "the recipient {a} is the sender; this would spend gas to move nothing"
            ),
        }
    }
}

impl std::error::Error for TransferError {}

/// Convert the amount once, here, and never again.
pub fn transfer_base_units(transfer: &Transfer) -> Result<u64, TransferError> {
    let amount = decimal_to_base_units(&transfer.amount, transfer.decimals)
        .map_err(TransferError::Amount)?;
    if amount == 0 {
        return Err(TransferError::ZeroAmount);
    }
    Ok(amount)
}

/// Split an exact amount off the gas coin and send it.
///
/// SUI only — for any other coin the caller must supply the coin object, since gas is always SUI.
pub fn build_transfer_sui(
    tx: &mut TransactionBuilder,
    sender: Address,
    transfer: &Transfer,
) -> Result<u64, TransferError> {
    if transfer.recipient == sender {
        return Err(TransferError::SelfTransfer(sender));
    }
    let amount = transfer_base_units(transfer)?;

    let value = tx.pure(&amount);
    let gas = tx.gas();
    let coin = tx
        .split_coins(gas, vec![value])
        .into_iter()
        .next()
        .expect("split_coins returns one result per requested amount");

    let recipient = tx.pure(&transfer.recipient);
    tx.transfer_objects(vec![coin], recipient);
    Ok(amount)
}

/// Send an already-obtained coin, whole.
///
/// Used to consume the output of a swap or a gated spend — the coin exists as a command result, so
/// there is nothing to split and nothing to convert.
pub fn transfer_coin(tx: &mut TransactionBuilder, coin: Argument, recipient: Address) {
    let to = tx.pure(&recipient);
    tx.transfer_objects(vec![coin], to);
}

/// The targets a SUI transfer emits. Both are framework commands rather than Move calls, so a
/// pinned sequence names them as the builder's own steps.
pub fn expected_transfer_targets() -> Vec<String> {
    vec!["SplitCoins".into(), "TransferObjects".into()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use sui_sdk_types::Digest;
    use sui_transaction_builder::ObjectInput;

    fn addr(n: u8) -> Address {
        format!("0x{n:064x}").parse().unwrap()
    }

    fn builder() -> TransactionBuilder {
        let mut tx = TransactionBuilder::new();
        tx.set_sender(addr(9));
        tx.set_gas_budget(50_000_000);
        tx.set_gas_price(1_000);
        tx.add_gas_objects([ObjectInput::owned(addr(0x0a), 1, Digest::ZERO)]);
        tx
    }

    fn sui(amount: &str) -> Transfer {
        Transfer {
            recipient: addr(7),
            amount: amount.into(),
            decimals: 9,
        }
    }

    #[test]
    fn a_decimal_amount_becomes_exact_base_units() {
        assert_eq!(transfer_base_units(&sui("1.5")).unwrap(), 1_500_000_000);
        assert_eq!(transfer_base_units(&sui("0.000000001")).unwrap(), 1);
    }

    /// The failure the reference implementation shipped: a value that no double represents exactly.
    #[test]
    fn an_amount_a_float_would_round_is_exact_here() {
        assert_eq!(
            transfer_base_units(&sui("2362.123456789")).unwrap(),
            2_362_123_456_789
        );
    }

    #[test]
    fn more_decimals_than_the_coin_has_is_refused_rather_than_truncated() {
        let result = transfer_base_units(&sui("0.0000000001"));
        assert!(
            matches!(result, Err(TransferError::Amount(_))),
            "silently dropping a digit changes the amount: {result:?}"
        );
    }

    #[test]
    fn a_transfer_of_zero_is_refused() {
        assert_eq!(
            transfer_base_units(&sui("0")),
            Err(TransferError::ZeroAmount)
        );
    }

    #[test]
    fn sending_to_yourself_is_refused_before_gas_is_spent() {
        let mut tx = builder();
        let mut transfer = sui("1");
        transfer.recipient = addr(9);
        assert!(matches!(
            build_transfer_sui(&mut tx, addr(9), &transfer),
            Err(TransferError::SelfTransfer(_))
        ));
    }

    #[test]
    fn a_transfer_builds_into_a_real_transaction() {
        let mut tx = builder();
        let amount = build_transfer_sui(&mut tx, addr(9), &sui("1.5")).expect("should build");
        assert_eq!(amount, 1_500_000_000);
        tx.try_build().expect("valid transaction");
    }

    #[test]
    fn the_expected_targets_name_both_commands() {
        assert_eq!(
            expected_transfer_targets(),
            vec!["SplitCoins", "TransferObjects"]
        );
    }
}
